use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
// tokio's clock, not std's: it is the one the timers here run on, and the one
// a test can pause, so a settle window and the tick that checks it agree.
use tokio::time::Instant;

use crate::MIN_HERDR_PROTOCOL;
use crate::dispatch::dispatch_with;
use crate::herdr::{
    AgentInfo, AgentStatus, CallError, Connector, ConnectorExt, EventStream,
    subscription_agent_status, subscription_lifecycle,
};
use crate::store::{Store, TaskFilter};
use crate::sync::Recover;
use crate::task::{Observed, Task, TaskState, TaskSummary, next_state};

/// A herdr request that got no answer within `request_timeout`.
#[derive(Debug, thiserror::Error)]
#[error("{0} timed out after {1:?}")]
struct TimedOut(&'static str, Duration);

/// What `Actor::prompt_by_socket` did with a pending prompt.
enum BySocket {
    /// The session took it; the agent as `agent.list` showed it just before.
    Sent(Box<AgentInfo>),
    /// The agent is launching or blocked: send nothing yet.
    Wait,
    /// Type it, as for an agent with no socket.
    Type,
}

/// Auto-close found the agent of a done task no longer idle: not a failure,
/// the task is simply not done any more.
#[derive(Debug, thiserror::Error)]
#[error("the agent is {0:?}, not idle")]
struct NotIdle(AgentStatus);

/// The pane a task recorded holds an agent under another name: herdr handed
/// the pane id out again after the task's own pane closed. Not the task's pane
/// any more, so nothing of it is closed.
#[derive(Debug, thiserror::Error)]
#[error("pane {pane} now holds agent {agent:?}, not {expected}")]
struct PaneReused {
    pane: String,
    agent: Option<String>,
    expected: String,
}

/// Auto-close's fresh check found the agent of a done task idle, matching the
/// row, but its `completion_seq` (or `state_change_seq`, absent that) has
/// moved past the row's `last_completion_seq`: a whole work cycle finished
/// since, unseen by reconcile. Not a failure; the row needs reconcile's
/// settle window to catch up before it can be closed.
#[derive(Debug, thiserror::Error)]
#[error(
    "the agent's sequence moved past the row's baseline {baseline} (state_change_seq {state_change_seq}, completion_seq {completion_seq:?})"
)]
struct SequenceMoved {
    state_change_seq: u64,
    completion_seq: Option<u64>,
    baseline: u64,
}

/// Auto-close found a done row with no `last_completion_seq`: one written
/// before pastor recorded a baseline. With nothing to compare against, the
/// agent's sequence is recorded as the baseline and this pass skips the task;
/// the next pass closes it if the sequence has not moved since.
#[derive(Debug, thiserror::Error)]
#[error("the row had no completion baseline; recorded {seq} from herdr")]
struct BaselineSeeded {
    seq: u64,
}

/// Whether an error from the connected loop means the machine is gone. Only a
/// transport failure or a request that never answered does; a herdr API error
/// or a local store error leaves the machine reachable, and reporting it as
/// `machine.lost` would announce an outage that is not happening.
fn is_outage(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.is::<TimedOut>()
            || e.downcast_ref::<CallError>()
                .is_some_and(CallError::is_transport)
    })
}

/// The advice given when `--remove-worktree` has no herdr workspace left to
/// remove the checkout by: the checkout itself may still be on disk, so
/// removing it is left to the operator.
fn manual_worktree_cleanup(display_id: &str) -> String {
    format!(
        "task {display_id} has no open workspace left to remove the worktree from; remove the checkout with `git worktree remove`"
    )
}

/// The note on a task whose worktree auto-close kept, and why. The branch is
/// the one the checkout was recorded on: a retry reopens an older checkout
/// and drops `spec.branch`, so the requested one may not be where the work is.
/// Whether `path`, as a task row recorded it, may be handed to `git worktree
/// remove` at all: absolute, with no `.` or `..` part and at least three
/// parts, so a damaged row never names `/` or a home directory.
fn sweepable_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    parts.len() >= 3 && parts.iter().all(|p| *p != "." && *p != "..")
}

fn worktree_kept_note(t: &Task, name: &str, why: &str) -> String {
    let branch = t
        .spec
        .checkout
        .as_ref()
        .map(|c| c.branch.clone())
        .or_else(|| t.spec.branch.clone())
        .unwrap_or_else(|| format!("pastor/{name}"));
    format!(
        "worktree kept: {why} on branch {branch}{}; remove the checkout with `git worktree remove` once it is saved",
        t.spec
            .repo
            .as_deref()
            .map(|r| format!(" of {r}"))
            .unwrap_or_default()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelState {
    Connecting,
    Connected,
    Reconnecting,
    /// Requests answer but `events.subscribe` will not open: tasks are tracked
    /// by `agent.list` every `poll_every` until a subscribe succeeds.
    Polling,
    Incompatible,
}

impl ChannelState {
    /// May the dispatcher place a task here? Only states in which requests are
    /// known to answer.
    pub fn accepts_dispatch(&self) -> bool {
        matches!(self, ChannelState::Connected | ChannelState::Polling)
    }
}

impl MachineStatus {
    /// The flocks the machine is in: `flocks`, else the one `flock` a head
    /// from before many flocks sends, else the default.
    pub fn flock_names(&self) -> Vec<String> {
        if !self.flocks.is_empty() {
            self.flocks.iter().map(|f| f.name.clone()).collect()
        } else {
            vec![
                self.flock
                    .clone()
                    .unwrap_or_else(|| crate::config::flock::DEFAULT_FLOCK.into()),
            ]
        }
    }

    pub fn in_flock(&self, flock: &str) -> bool {
        self.flock_names().iter().any(|f| f == flock)
    }
}

impl std::fmt::Display for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChannelState::Connecting => "connecting",
            ChannelState::Connected => "connected",
            ChannelState::Reconnecting => "reconnecting",
            ChannelState::Polling => "polling",
            ChannelState::Incompatible => "incompatible",
        })
    }
}

/// One flock a machine is in: its number there (`None`: the machine's own
/// limits, from the old `flock` key or the default), as a share and a max
/// (`FlockNumber`), and how many of the flock's live tasks run on the
/// machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlockSeat {
    pub name: String,
    /// Up to how many live tasks the flock takes a slot as usual; `None`
    /// with a `max` is a head from before shares, whose number was a plain
    /// ceiling, so the share is the max.
    #[serde(default)]
    pub share: Option<u32>,
    #[serde(default)]
    pub max: Option<u32>,
    #[serde(default)]
    pub live: usize,
}

impl FlockSeat {
    /// `name`'s seat with `number` (`None`: the machine's own limits) and
    /// `live` of its tasks.
    pub fn new(
        name: &str,
        number: Option<crate::config::flock::FlockNumber>,
        live: usize,
    ) -> FlockSeat {
        FlockSeat {
            name: name.to_string(),
            share: number.map(|n| n.share()),
            max: number.map(|n| n.max()),
            live,
        }
    }

    /// Is the flock under its number here? Its max, which job slots and
    /// burst never pass.
    pub fn has_room(&self) -> bool {
        self.max.is_none_or(|n| self.live < n as usize)
    }

    /// Is the flock at 0 here: in it, but starting nothing, to drain it?
    pub fn drained(&self) -> bool {
        self.max == Some(0)
    }

    /// Is the flock under its share here, so it takes a free slot without
    /// looking at who else waits? A seat with no number is.
    pub fn under_share(&self) -> bool {
        self.share
            .or(self.max)
            .is_none_or(|n| self.live < n as usize)
    }

    /// The number as `flock list` and `machine list` write it: `2`, or
    /// `2/4` for a share of 2 and a max of 4. `None` with no number.
    pub fn number_label(&self) -> Option<String> {
        let max = self.max?;
        Some(match self.share.filter(|s| *s != max) {
            Some(share) => format!("{share}/{max}"),
            None => max.to_string(),
        })
    }
}

/// What a machine has free, as `Connector::room` last read it: disk under
/// `$HOME` and `/tmp` and memory, in KiB (`df -Pk`'s 1024-byte blocks,
/// `/proc/meminfo`'s kB). Each number is `None` where the machine could not
/// say: a home `df` cannot read, memory on macOS, which has no
/// `/proc/meminfo`. Read at connect and every `MachineSettings::room_every`;
/// a probe that fails or times out sets `error` and keeps the numbers last
/// read with their `asked_at`, so a reader can tell how old they are. A
/// floor on these numbers is another card's; this one only keeps them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    pub home_free: Option<u64>,
    pub tmp_free: Option<u64>,
    pub mem_free: Option<u64>,
    /// When the numbers were read. `None` until a probe answers.
    #[serde(default)]
    pub asked_at: Option<DateTime<Utc>>,
    /// What the last probe failed with, `None` once one answers again.
    #[serde(default)]
    pub error: Option<String>,
}

/// Whether the machine's agents last reached the head, as the `reach` row of
/// a `machine check` the head ran said: `ok` when the row passed, not when
/// it failed. Nothing but a later check changes it, so a machine the head
/// never checked has none and is placed as ever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reach {
    pub ok: bool,
    pub at: DateTime<Utc>,
    /// What ssh said, when it failed.
    #[serde(default)]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineStatus {
    pub name: String,
    /// `Connector::host`. Defaulted so a CLI can still read a head that
    /// predates the field.
    #[serde(default)]
    pub host: String,
    pub endpoint: String,
    pub channel: ChannelState,
    pub herdr_version: Option<String>,
    /// `Connector::pastor_version`, asked at each connect and again every
    /// `MachineSettings::version_every` while connected or polling. Defaulted so a
    /// CLI can still read a head that predates the field.
    #[serde(default)]
    pub pastor_version: Option<String>,
    /// When the machine last answered the version question, so a fresh
    /// install that still shows the old version can be told from a head
    /// that has not asked yet. "No pastor there" is an answer, so a `-`
    /// version with a time means checked and not installed. `None` until
    /// it answers, and from a head that predates the field.
    #[serde(default)]
    pub pastor_read_at: Option<DateTime<Utc>>,
    /// The harness versions the machine's pins name (`harness`), as last
    /// read, with whether each drifts from its pin; read at each connect and
    /// every `version_every` with the pastor version. Empty with no pins,
    /// and from a head that predates it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harness: Vec<crate::harness::HarnessStatus>,
    /// What the machine has free (`Connector::room`), read at each connect
    /// and again every `MachineSettings::room_every` while connected or
    /// polling. `None` until the first probe, and for a machine that cannot
    /// say (a `command` machine). Defaulted so a CLI can still read a head
    /// that predates the field.
    #[serde(default)]
    pub room: Option<Room>,
    /// The last `reach` result of a `machine check` of it. Defaulted so a
    /// CLI can still read a head that predates the field.
    #[serde(default)]
    pub reach: Option<Reach>,
    pub protocol: Option<u32>,
    pub error: Option<String>,
    pub live: usize,
    /// How many of `live` are tasks from jobs (`Task::from_job`). Defaulted
    /// so a CLI can still read a head that predates the field.
    #[serde(default)]
    pub live_jobs: usize,
    pub max_agents: u32,
    /// `MachineConfig::job_slots` and `burst`. Defaulted to 0, none, so a
    /// CLI reading a head that predates them shows none.
    #[serde(default)]
    pub job_slots: u32,
    #[serde(default)]
    pub burst: u32,
    pub tags: Vec<String>,
    /// The machine is cordoned (`pastor machine cordon`): it takes no new
    /// task. Filled in by `Fleet::statuses` from the store; `None` from an
    /// actor, and from a head that predates cordons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cordon: Option<crate::store::Cordon>,
    /// Agents named like a task (`t-<id>`) that no open task on this machine
    /// owns: the row is failed, closed or gone (a dispatch that failed after
    /// `agent.start`, a daemon killed mid-dispatch, a pruned row). Counted in
    /// `live`, never closed unless `pastor task close` asks.
    #[serde(default)]
    pub orphans: Vec<String>,
    /// The live tasks here that `task run --now` started past the
    /// machine's limits, like `t-9`; `live` counts them. Filled in by
    /// `Fleet::statuses` from the store; empty from an actor, and from a
    /// head that predates `--now`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub now: Vec<String>,
    /// The flock the machine is in, as the head last applied flock.toml.
    /// The actor does not know it; `Fleet::statuses` fills it in. `None`
    /// from an actor, and from a head that predates flocks.
    #[serde(default)]
    pub flock: Option<String>,
    /// Every flock the machine is in, the one in `flock` among them, with
    /// its number and live tasks there. `Fleet::statuses` fills it in;
    /// empty from an actor, and from a head that predates it (read it
    /// through `flock_names`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flocks: Vec<FlockSeat>,
    /// The actor's live tasks by the flock stored on each (`None`: a row
    /// from before flocks, in the default flock), for `Fleet` to count each
    /// flock's tasks against its number. Not sent: `flocks` carries it.
    #[serde(skip)]
    pub live_by_flock: Vec<(Option<String>, usize)>,
    /// The actor was stopped by a reload that took this machine out of the
    /// flock, but it has not ended yet (`Fleet::statuses` fills it in). A
    /// caller that grants access by flock membership must treat this machine
    /// as removed, not as still in `flock`. `false` from an actor, and from
    /// a head that predates the field.
    #[serde(default)]
    pub shutting_down: bool,
    /// The permission profile a task here runs under when it names none:
    /// the machine's own, else its flock's, else `[defaults]`. The actor
    /// does not know it; `Fleet::statuses` fills it in. `None` when no
    /// layer names one, and from a head that predates profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The machine's `description` in flock.toml, as the head last applied
    /// it; `Fleet::statuses` fills it in. `None` from an actor, and from a
    /// head that predates it.
    #[serde(default)]
    pub description: Option<String>,
    /// `MachineConfig::reserved`: the slots dispatch keeps free of its own
    /// tasks; `Fleet::statuses` fills it in. 0 from an actor, and from a
    /// head that predates it.
    #[serde(default)]
    pub reserved: u32,
    /// The head reaches the machine over `ssh` and has no `head_address`,
    /// so its agents cannot run `pastor task done` (`daemon::is_cut_off`).
    /// `Fleet::statuses` fills it in; `false` from an actor, and from a
    /// head that predates it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_head_address: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MachineSettings {
    pub settle: Duration,
    pub reconcile_every: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Bound on any single herdr request (dispatch, read, ...), covering the
    /// connect as well as the reply. A machine that does not answer within this
    /// window is treated as lost: the request fails and the actor reconnects.
    pub request_timeout: Duration,
    /// How long a dispatch waits between `agent.start` and a prompt herdr
    /// accepts. Must stay below `request_timeout`, which bounds the dispatch as
    /// a whole: otherwise a slow agent surfaces as "request timed out" (and a
    /// reconnect) instead of the readiness failure it is.
    pub agent_ready_timeout: Duration,
    /// While `Polling`, how often `agent.list` reconciles. The daemon passes its
    /// `tick`, so a polling machine is reconciled once per tick.
    pub poll_every: Duration,
    /// How long a `done` task keeps its pane before auto-close takes it
    /// (`close_done_after` in `pastor.toml`). `None` turns auto-close off.
    /// While connected, the check runs after each reconcile and, for a grace
    /// shorter than `reconcile_every`, every `close_done_after` as well.
    pub close_done_after: Option<Duration>,
    /// How long a `failed` or `stale` task's stopped agent, or an orphaned
    /// one, keeps its pane before auto-close takes it (`close_failed_after`
    /// in `pastor.toml`). `None` keeps those panes. Checked with
    /// `close_done_after`.
    pub close_failed_after: Option<Duration>,
    /// While connected or polling, how often the machine is asked for its
    /// pastor version again, so an upgrade shows without a reconnect. Checked
    /// after each reconcile (`reconcile_every` connected, `poll_every`
    /// polling), so the real interval rounds up to that tick. `version_every`
    /// in pastor.toml, two minutes by default.
    pub version_every: Duration,
    /// While connected or polling, how often the machine's room (free disk
    /// and memory, `MachineStatus::room`) is read again, whether or not a
    /// floor is set on it: the numbers move with every checkout and build.
    /// Checked after each reconcile, like `version_every`. Sixty seconds by
    /// default; not in `pastor.toml` yet.
    pub room_every: Duration,
    /// `[agents]` in `pastor.toml`: the startup dialogs each agent may
    /// show and how they are answered (`Agents::dialogs`).
    pub agents: crate::config::Agents,
    /// `head_address` in `pastor.toml`, for a machine other than the head's
    /// own (`daemon::actor_settings`): its agents get it as `ipc::HEAD_ENV`.
    pub head_address: Option<String>,
    /// The machine is reached over `ssh` and the head has no `head_address`
    /// (`daemon::actor_settings`): its agents get no `ipc::HEAD_ENV`, so
    /// their `pastor task done` cannot reach the head, and a round here that
    /// ends with no summary is `Outcome::NoHead`.
    pub cut_off_from_head: bool,
    /// `[limits]` in `pastor.toml`, on the head: a task whose agent stops
    /// on a usage limit goes `waiting` and its limit goes in the table.
    /// `None` on a headless serve, which keeps no table: its tasks settle as
    /// if no limit were read.
    pub limits: Option<crate::config::LimitsConfig>,
    /// The machine's own permission profile (its `[[machine]]` profile, else
    /// its flock's, else `[defaults]`), its lists in Claude Code's patterns:
    /// what the agy provisioner makes agy's settings there say before a
    /// profiled agy launch (`provision::agy`). `None` with no profile. Set
    /// per machine by `Fleet::own_policy`, so a change of profile replaces
    /// only that machine's actor.
    pub own_policy: Option<crate::config::agy::Policy>,
    /// The harness versions the machine's flock (or the machine) pins,
    /// per kind (`Flock::pins_for`): the actor reads the version of each
    /// and reports the drift (`MachineStatus::harness`). Empty with no pins.
    /// Set per machine by `Fleet::settings_of`, so a change of pins replaces
    /// only that machine's actor.
    pub harness_pins: crate::harness::Pins,
    /// The harness manifests: how to ask a machine for each pinned kind's
    /// version.
    pub harnesses: std::sync::Arc<crate::harness_manifest::Harnesses>,
    /// Asked before a task starts: whether the machine's bundles allow it.
    /// Set by `Fleet::settings_of` from the head's keeper; none elsewhere.
    pub bundle_gate: crate::bundle::keep::GateHandle,
    /// Places a task's workdir set before its agent starts, and takes it out
    /// when its worktree goes. Set by `Fleet::settings_of` from the head on
    /// a machine it can place sets on; none elsewhere.
    pub workdir_gate: crate::bundle::workdir_task::GateHandle,
    /// The env layers under an agent's own that reach this machine
    /// (`EnvLayers::of`). Set per machine by `Fleet::settings_of`, so a
    /// change replaces only the actors of the machines it reaches.
    pub env: crate::config::EnvLayers,
}

impl Default for MachineSettings {
    fn default() -> Self {
        MachineSettings {
            settle: Duration::from_secs(10),
            reconcile_every: Duration::from_secs(60),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
            agent_ready_timeout: Duration::from_secs(30),
            poll_every: Duration::from_secs(10),
            close_done_after: Some(Duration::from_secs(5)),
            close_failed_after: Some(Duration::from_secs(5)),
            version_every: Duration::from_secs(2 * 60),
            room_every: Duration::from_secs(60),
            agents: crate::config::Agents::default(),
            head_address: None,
            cut_off_from_head: false,
            limits: None,
            own_policy: None,
            harness_pins: Default::default(),
            harnesses: Default::default(),
            bundle_gate: Default::default(),
            workdir_gate: Default::default(),
            env: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PastorEvent {
    pub kind: String,
    #[serde(default)]
    pub task_id: Option<i64>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub job: Option<String>,
    /// What the event carries beyond its ids: for `task.input`, the key
    /// names sent and the length of any text, never the text itself.
    #[serde(default)]
    pub detail: Option<serde_json::Value>,
    /// On `task.done` and `task.failed`: how the round that just ended
    /// ended, taken at the moment the event was queued. The event log's
    /// build step trusts this rather than re-reading the store, which by
    /// then may hold a later round's summary (a reopen, or `task done
    /// --summary` landing on an already-done task before this event was
    /// processed).
    #[serde(default)]
    pub summary: Option<TaskSummary>,
}

pub enum MachineCommand {
    Dispatch {
        task_id: i64,
        reply: oneshot::Sender<anyhow::Result<Task>>,
    },
    Read {
        task_id: i64,
        lines: u32,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },
    /// `pastor task close`: close the task's pane (with `remove_worktree`,
    /// remove its worktree instead, which closes the pane with it), then mark
    /// the row closed. Also closes an orphaned agent `t-<task_id>`; with no
    /// row at all the reply is an `OrphanClosed` error.
    Close {
        task_id: i64,
        remove_worktree: bool,
        reply: oneshot::Sender<anyhow::Result<Task>>,
    },
    /// `pastor task send`: type into the pane of a live task (starting with
    /// a pane, running or blocked). Replies with the row as it was sent to.
    Send {
        task_id: i64,
        input: SendInput,
        reply: oneshot::Sender<anyhow::Result<Task>>,
    },
    /// `pastor task done`: the agent says it is finished. Marks the task
    /// `done` and `ended`, so it stays done while the agent finishes its
    /// turn and auto-close takes its pane after `close_done_after`.
    /// `summary` ends the round with what the agent said
    /// (`Store::end_round`), or replaces the last round's on a task pastor
    /// already found done.
    End {
        task_id: i64,
        summary: Option<String>,
        by: EndBy,
        reply: oneshot::Sender<anyhow::Result<Task>>,
    },
    /// Pause a running `low` Claude task for critical task `for_task`:
    /// interrupt its agent, close its pane (its worktree stays) and mark it
    /// `paused` (see `Task::pause`). Refused with `PauseRefused` for a task
    /// that cannot be paused (`task::why_not_pausable`).
    Pause {
        task_id: i64,
        for_task: i64,
        reply: oneshot::Sender<anyhow::Result<Task>>,
    },
    /// Before a task leaves this machine: tell its agent to commit and push
    /// its work (`task::MOVE_SAVE_PROMPT`), through its Claude session's
    /// inbox socket when that takes it and typed otherwise, then reply once
    /// it has finished that prompt and gone idle. Refused (`SendRefused`)
    /// with `task_not_live` for a task with no pane here, `agent_busy` for
    /// an agent still working and `save_blocked` for one waiting on a
    /// question; `save_timeout` when preflight, the prompt, or the wait does
    /// not clear within `timeout`; `save_blocked` too for an agent that
    /// stopped to ask a question instead of finishing. The row and the
    /// pane are left as they are either way, and the task is excluded from
    /// `auto_close_done` until the save resolves, however it resolves.
    SaveWork {
        task_id: i64,
        timeout: Duration,
        reply: oneshot::Sender<anyhow::Result<Task>>,
    },
    /// Start a paused task again on its own session (`dispatch::resume`).
    Resume {
        task_id: i64,
        reply: oneshot::Sender<anyhow::Result<Task>>,
    },
    /// Run the saved-trust check (`auto_trust`) now rather than at the next
    /// reconcile: the trust a pull machine's serve keeps for it changed
    /// while a task was already blocked on its prompt (`shepherd::Puller`).
    AutoTrust {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// `pastor task sweep`: the worktrees pastor kept on this machine for
    /// tasks that ended, listed, and with `apply` removed when nothing
    /// forbids it (`Actor::run_sweep`).
    Sweep {
        apply: bool,
        reply: oneshot::Sender<anyhow::Result<Vec<SweepEntry>>>,
    },
}

/// One worktree `pastor task sweep` looked at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepEntry {
    pub machine: String,
    pub path: String,
    pub branch: String,
    /// The tasks whose rows record the worktree.
    pub tasks: Vec<String>,
    pub action: SweepAction,
    /// Why it is kept; none otherwise.
    #[serde(default)]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepAction {
    Removed,
    /// A dry run found nothing that forbids removing it.
    WouldRemove,
    Kept,
}

/// A `task run --now` dispatch, sent on the actor's urgent channel
/// (`MachineHandle::dispatch_now`) rather than behind the other commands:
/// the actor takes it even while it is starting another task
/// (`Actor::run_dispatch`), so that task's startup never holds it up.
pub struct NowDispatch {
    pub task_id: i64,
    pub reply: oneshot::Sender<anyhow::Result<Task>>,
}

/// Why `Pause` left a task alone.
#[derive(Debug, thiserror::Error)]
#[error("{task} cannot be paused: {why}")]
pub struct PauseRefused {
    pub task: String,
    pub why: String,
}

/// What `pastor task send` types into a task's pane: `text` first, then
/// Enter if `enter`, then each of `keys` in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendInput {
    #[serde(default)]
    pub text: Option<String>,
    /// Press Enter after `text`. Ignored without text.
    #[serde(default)]
    pub enter: bool,
    /// Named keys (`Enter`, `Down`, `esc`, `ctrl+c`), pressed after the text.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Send the agent's trust keys instead (`[agents] trust_keys`), and save
    /// the task's (machine, repo) as trusted. Never with text or keys.
    #[serde(default)]
    pub trust: bool,
    /// Answer the picker on screen by the text of one of its options:
    /// arrows to it, then Enter. Never with text, keys or trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<String>,
}

impl SendInput {
    /// The keys pressed, in order: Enter after text if asked, then `keys`.
    pub fn key_sequence(&self) -> Vec<String> {
        let enter = (self.text.is_some() && self.enter).then(|| "Enter".to_string());
        enter.into_iter().chain(self.keys.iter().cloned()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.keys.is_empty()
            && !self.trust
            && self.pick.as_deref().is_none_or(|p| p.trim().is_empty())
    }

    /// What `task.input` records for text and keys: the key names and the
    /// length of the text, never the text, which may hold a secret. A pick
    /// is recorded by `run_send`, with the keys it worked out.
    pub fn detail(&self) -> serde_json::Value {
        let mut d = serde_json::Map::new();
        if let Some(text) = &self.text {
            d.insert("text_len".into(), text.chars().count().into());
        }
        let keys = self.key_sequence();
        if !keys.is_empty() {
            d.insert("keys".into(), keys.into());
        }
        d.into()
    }
}

/// `Send` refused before it typed anything, `End` before it wrote
/// anything, or `SaveWork` refused or timed out, for a reason the CLI
/// reports under its own code (`task_not_live`, ...).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SendRefused {
    pub code: &'static str,
    pub message: String,
}

/// `Close` found no task row, only an orphaned agent by that name, and closed
/// it. There is no `Task` to reply with, so this is how success reads; the
/// daemon turns it into a plain message.
#[derive(Debug, thiserror::Error)]
#[error("closed orphaned agent {agent} on {machine}; it had no task row")]
pub struct OrphanClosed {
    pub agent: String,
    pub machine: String,
}

/// What was running under a pane about to close, listed before the close
/// (`Actor::find_leftovers`).
struct Leftovers {
    task_id: Option<i64>,
    pane: String,
    name: String,
    found: Vec<crate::reap::Candidate>,
    /// The pane is closed already, whatever else the close does: the agent
    /// is gone and what it left may be ended even if the close fails later.
    pane_closed: bool,
    /// The checkout is gone: only then are the processes found by working in
    /// it ended. A checkout that stays is somebody's, and what works in it
    /// with it.
    checkout_gone: bool,
}

/// The machine's actor was stopped (a flock reload removed or replaced the
/// machine) before it answered. An aborted actor stuck in a poll never reads
/// its queue again, so a request fails with this instead of waiting for it.
#[derive(Debug, thiserror::Error)]
#[error("machine {machine} is shutting down; try again later")]
pub struct ActorStopped {
    pub machine: String,
}

#[derive(Clone)]
pub struct MachineHandle {
    pub name: String,
    pub max_agents: u32,
    /// Set by `with_slots`; 0 from `spawn_machine`.
    pub job_slots: u32,
    pub burst: u32,
    pub tags: Vec<String>,
    pub tx: mpsc::Sender<MachineCommand>,
    /// The urgent channel, for `dispatch_now` only.
    pub now_tx: mpsc::Sender<NowDispatch>,
    pub status: Arc<RwLock<MachineStatus>>,
    /// The actor task, for `shutdown`. `None` only for handles built by hand
    /// in tests, which have no actor.
    pub task: Option<Arc<ActorTask>>,
}

/// How long `shutdown` waits for an aborted actor to end. An abort lands at
/// the actor's next await, so this is only reached if a poll blocks.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// How often a prompt the agent did not take is sent again before the task
/// is left `blocked` for a human (see `Actor::unseen_prompt`).
const MAX_PROMPT_RESENDS: u8 = 2;

/// The running actor, shared by every clone of its handle. The join handle
/// is kept, not just an abort handle, because aborting only asks the task to
/// stop: until it has ended it can still write a row.
pub struct ActorTask {
    abort: tokio::task::AbortHandle,
    /// Set by `shutdown` before it aborts. Requests wait on it next to their
    /// reply: an aborted actor stuck in a poll never drops its queue, so its
    /// replies would otherwise never come.
    stopping: watch::Sender<bool>,
    /// `None` once a `shutdown` has seen the task end. A tokio mutex, held
    /// across the wait, so a second caller waits too instead of guessing.
    join: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// What `MachineHandle::shutdown` saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum ShutdownOutcome {
    /// The actor task has ended: it will not write another row.
    Finished,
    /// Aborted, but still inside a poll after `SHUTDOWN_WAIT`. It may still
    /// write rows until it ends, so nothing may take its place yet. Calling
    /// `shutdown` again waits again.
    StillRunning,
}

impl MachineHandle {
    /// Give the machine `job_slots` and `burst` (`MachineConfig`), on the
    /// handle for the picker and in the status for `machine list`.
    pub fn with_slots(mut self, job_slots: u32, burst: u32) -> MachineHandle {
        self.job_slots = job_slots;
        self.burst = burst;
        let mut s = self.status.write().recover();
        s.job_slots = job_slots;
        s.burst = burst;
        drop(s);
        self
    }

    /// Stop the actor and wait until its task has ended. A flock reload calls
    /// this for a machine it removes or replaces, before it spawns the
    /// replacement, so a removed actor cannot write a row after removal and a
    /// retargeted machine never has two actors racing on its tasks.
    ///
    /// Nothing on the machine is touched: agents keep running, rows keep their
    /// state, and a replacement actor reconciles them. A request in flight,
    /// or sent from now on, fails with `ActorStopped`. Waits at most `SHUTDOWN_WAIT`; if the
    /// task has not ended by then it warns and returns `StillRunning`, and
    /// keeps the task so a later call can wait for it again.
    pub async fn shutdown(&self) -> ShutdownOutcome {
        let Some(task) = &self.task else {
            return ShutdownOutcome::Finished;
        };
        task.stopping.send_replace(true);
        task.abort.abort();
        let mut join = task.join.lock().await;
        let Some(handle) = join.as_mut() else {
            return ShutdownOutcome::Finished;
        };
        match tokio::time::timeout(SHUTDOWN_WAIT, handle).await {
            Ok(res) => {
                if let Err(err) = res
                    && !err.is_cancelled()
                {
                    tracing::warn!(machine = %self.name, %err, "actor ended with an error");
                }
                *join = None;
                ShutdownOutcome::Finished
            }
            Err(_) => {
                tracing::warn!(
                    machine = %self.name,
                    wait = ?SHUTDOWN_WAIT,
                    "actor did not stop in time; still waiting for it"
                );
                ShutdownOutcome::StillRunning
            }
        }
    }

    /// Whether the actor task has ended. True for a handle with no actor.
    pub fn actor_finished(&self) -> bool {
        self.task.as_ref().is_none_or(|t| t.abort.is_finished())
    }

    pub fn snapshot(&self) -> MachineStatus {
        self.status.read().recover().clone()
    }

    pub async fn dispatch(&self, task_id: i64) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        self.request(MachineCommand::Dispatch { task_id, reply }, rx)
            .await
    }

    /// `dispatch` for a `--now` task: the actor takes it at once, even while
    /// it is starting an earlier task.
    pub async fn dispatch_now(&self, task_id: i64) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        self.request_on(&self.now_tx, NowDispatch { task_id, reply }, rx)
            .await
    }

    pub async fn close(&self, task_id: i64, remove_worktree: bool) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        let cmd = MachineCommand::Close {
            task_id,
            remove_worktree,
            reply,
        };
        self.request(cmd, rx).await
    }

    pub async fn read(&self, task_id: i64, lines: u32) -> anyhow::Result<String> {
        let (reply, rx) = oneshot::channel();
        let cmd = MachineCommand::Read {
            task_id,
            lines,
            reply,
        };
        self.request(cmd, rx).await
    }

    pub async fn send(&self, task_id: i64, input: SendInput) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        let cmd = MachineCommand::Send {
            task_id,
            input,
            reply,
        };
        self.request(cmd, rx).await
    }

    /// `end_by` the task's own agent.
    pub async fn end(&self, task_id: i64, summary: Option<String>) -> anyhow::Result<Task> {
        self.end_by(task_id, summary, EndBy::Agent).await
    }

    /// See `MachineCommand::End`.
    pub async fn end_by(
        &self,
        task_id: i64,
        summary: Option<String>,
        by: EndBy,
    ) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        self.request(
            MachineCommand::End {
                task_id,
                summary,
                by,
                reply,
            },
            rx,
        )
        .await
    }

    /// See `MachineCommand::Pause`.
    pub async fn pause(&self, task_id: i64, for_task: i64) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        let cmd = MachineCommand::Pause {
            task_id,
            for_task,
            reply,
        };
        self.request(cmd, rx).await
    }

    /// See `MachineCommand::SaveWork`.
    pub async fn save_work(&self, task_id: i64, timeout: Duration) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        let cmd = MachineCommand::SaveWork {
            task_id,
            timeout,
            reply,
        };
        self.request(cmd, rx).await
    }

    /// See `MachineCommand::Resume`.
    pub async fn resume(&self, task_id: i64) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        self.request(MachineCommand::Resume { task_id, reply }, rx)
            .await
    }

    /// See `MachineCommand::AutoTrust`.
    pub async fn auto_trust(&self) -> anyhow::Result<()> {
        let (reply, rx) = oneshot::channel();
        self.request(MachineCommand::AutoTrust { reply }, rx).await
    }

    /// See `MachineCommand::Sweep`.
    pub async fn sweep(&self, apply: bool) -> anyhow::Result<Vec<SweepEntry>> {
        let (reply, rx) = oneshot::channel();
        self.request(MachineCommand::Sweep { apply, reply }, rx)
            .await
    }

    /// Send `cmd` and wait for its reply, or fail with `ActorStopped` once
    /// `shutdown` has begun. An aborted actor stuck in a poll neither reads
    /// the queue nor drops it, so without this a request sent just before
    /// the abort would wait for the caller's own timeout.
    async fn request<T>(
        &self,
        cmd: MachineCommand,
        rx: oneshot::Receiver<anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        self.request_on(&self.tx, cmd, rx).await
    }

    /// `request` on channel `tx`.
    async fn request_on<C, T>(
        &self,
        tx: &mpsc::Sender<C>,
        cmd: C,
        rx: oneshot::Receiver<anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let exchange = async {
            tx.send(cmd)
                .await
                .map_err(|_| anyhow::anyhow!("machine {} is gone", self.name))?;
            rx.await
                .map_err(|_| anyhow::anyhow!("machine {} dropped the request", self.name))?
        };
        let Some(task) = &self.task else {
            return exchange.await;
        };
        let mut stopping = task.stopping.subscribe();
        // `biased`, exchange first: an actor that has ended answers "is gone"
        // or "dropped the request" at once, as before.
        tokio::select! {
            biased;
            res = exchange => res,
            _ = stopping.wait_for(|s| *s) => Err(ActorStopped {
                machine: self.name.clone(),
            }
            .into()),
        }
    }
}

/// `live`, `live_jobs` and `live_by_flock` of `status` from `tasks`, the
/// pane-owning tasks on its machine (`Store::tasks_on_machine`). A done
/// task that keeps its pane is left out: its agent is idle and waits for a
/// person, so the machine should take new work. `task send` makes it run,
/// and count, again.
pub fn count_live(status: &mut MachineStatus, tasks: &[Task]) {
    let tasks: Vec<&Task> = tasks
        .iter()
        .filter(|t| !(t.state == TaskState::Done && t.spec.keeps_pane()))
        .collect();
    status.live = tasks.len();
    status.live_jobs = tasks.iter().filter(|t| t.from_job()).count();
    let mut by: Vec<(Option<String>, usize)> = Vec::new();
    for t in tasks {
        match by.iter_mut().find(|(f, _)| *f == t.flock) {
            Some((_, n)) => *n += 1,
            None => by.push((t.flock.clone(), 1)),
        }
    }
    status.live_by_flock = by;
}

/// What `machine list` shows as a pull machine's endpoint.
pub const PULL_ENDPOINT: &str = "pull: its own pastor serve asks the head for tasks";

/// The handle of a pull machine (`pull = true` in flock.toml): no actor and
/// no connection, since the head never reaches it. A request sent to it
/// fails at once ("is gone"); the fleet answers for it instead
/// (`Fleet::claim`, `Fleet::report`). Its status starts `connecting` until
/// its first claim.
pub fn pull_machine(name: String, max_agents: u32, tags: Vec<String>) -> MachineHandle {
    let (tx, _) = mpsc::channel(1);
    let (now_tx, _) = mpsc::channel(1);
    let status = Arc::new(RwLock::new(MachineStatus {
        cordon: None,
        reserved: 0,
        now: Vec::new(),
        description: None,
        no_head_address: false,
        name: name.clone(),
        host: "pull".into(),
        endpoint: PULL_ENDPOINT.into(),
        channel: ChannelState::Connecting,
        herdr_version: None,
        pastor_version: None,
        pastor_read_at: None,
        harness: vec![],
        room: None,
        reach: None,
        protocol: None,
        error: None,
        live: 0,
        live_jobs: 0,
        max_agents,
        job_slots: 0,
        burst: 0,
        tags: tags.clone(),
        orphans: vec![],
        flock: None,
        flocks: vec![],
        live_by_flock: vec![],
        shutting_down: false,
        profile: None,
    }));
    MachineHandle {
        name,
        max_agents,
        job_slots: 0,
        burst: 0,
        tags,
        tx,
        now_tx,
        status,
        task: None,
    }
}

pub fn spawn_machine(
    name: String,
    max_agents: u32,
    tags: Vec<String>,
    connector: Arc<dyn Connector>,
    store: Arc<Store>,
    settings: MachineSettings,
    events: broadcast::Sender<PastorEvent>,
) -> MachineHandle {
    let (tx, rx) = mpsc::channel(32);
    let (now_tx, now_rx) = mpsc::channel(8);
    let status = Arc::new(RwLock::new(MachineStatus {
        cordon: None,
        reserved: 0,
        now: Vec::new(),
        description: None,
        no_head_address: false,
        name: name.clone(),
        host: connector.host(),
        endpoint: connector.describe(),
        channel: ChannelState::Connecting,
        herdr_version: None,
        pastor_version: None,
        pastor_read_at: None,
        harness: vec![],
        room: None,
        reach: None,
        protocol: None,
        error: None,
        live: 0,
        live_jobs: 0,
        max_agents,
        job_slots: 0,
        burst: 0,
        tags: tags.clone(),
        orphans: vec![],
        flock: None,
        flocks: vec![],
        live_by_flock: vec![],
        shutting_down: false,
        profile: None,
    }));
    let actor = Actor {
        name: name.clone(),
        connector,
        store,
        settings,
        events,
        status: status.clone(),
        rx,
        now_rx: Some(now_rx),
        pending_done: HashMap::new(),
        leftovers: None,
        dialog_answered: HashMap::new(),
        unseen_prompt: HashMap::new(),
        idle_agents: HashSet::new(),
        picker_read: HashSet::new(),
        rate_retries: HashMap::new(),
        was_connected: false,
        failures: 0,
        lost_announced: false,
        saving: Arc::new(Mutex::new(HashMap::new())),
        orphans: vec![],
        stopped_since: HashMap::new(),
        version_asked_at: Instant::now(),
        room_asked_at: Instant::now(),
    };
    let task = tokio::spawn(actor.run());
    MachineHandle {
        name,
        max_agents,
        job_slots: 0,
        burst: 0,
        tags,
        tx,
        now_tx,
        status,
        task: Some(Arc::new(ActorTask {
            abort: task.abort_handle(),
            stopping: watch::Sender::new(false),
            join: tokio::sync::Mutex::new(Some(task)),
        })),
    }
}

/// The actor's next command, a `--now` dispatch (`NowDispatch`) ahead of
/// the rest. `None` once the command channel is closed.
async fn next_command(
    rx: &mut mpsc::Receiver<MachineCommand>,
    now_rx: &mut Option<mpsc::Receiver<NowDispatch>>,
) -> Option<MachineCommand> {
    tokio::select! {
        biased;
        Some(now) = recv_now(now_rx) => Some(MachineCommand::Dispatch {
            task_id: now.task_id,
            reply: now.reply,
        }),
        cmd = rx.recv() => cmd,
    }
}

/// The next `--now` dispatch; never, while `run_dispatch` holds the channel.
async fn recv_now(now_rx: &mut Option<mpsc::Receiver<NowDispatch>>) -> Option<NowDispatch> {
    match now_rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The first of `starting` to finish, taken out of it. Pending while it is
/// empty.
async fn next_started<T>(
    starting: &mut Vec<std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + '_>>>,
) -> T {
    std::future::poll_fn(|cx| {
        for i in 0..starting.len() {
            if let std::task::Poll::Ready(v) = starting[i].as_mut().poll(cx) {
                drop(starting.swap_remove(i));
                return std::task::Poll::Ready(v);
            }
        }
        std::task::Poll::Pending
    })
    .await
}

struct Actor {
    name: String,
    connector: Arc<dyn Connector>,
    store: Arc<Store>,
    settings: MachineSettings,
    events: broadcast::Sender<PastorEvent>,
    status: Arc<RwLock<MachineStatus>>,
    rx: mpsc::Receiver<MachineCommand>,
    /// The urgent channel (`NowDispatch`). `None` only while `run_dispatch`
    /// holds it to take `--now` dispatches during another task's startup.
    now_rx: Option<mpsc::Receiver<NowDispatch>>,
    /// task id -> (the agent's `state_change_seq` when it was seen idle, if the
    /// observation carried one; when). Confirmed as Done after `settle` if the
    /// agent is still idle at that same sequence; with no sequence, the first
    /// check records one and starts the window over.
    pending_done: HashMap<i64, (Option<u64>, Instant)>,
    /// What a close under way found running (`find_leftovers`), ended by
    /// `run_close` if the close goes through.
    leftovers: Option<Leftovers>,
    /// task id -> when its startup prompt was answered: its trust keys went
    /// in, or, answered by a person at the pane, when the agent was first
    /// seen out of `blocked`. Claude redraws for a moment after its trust
    /// dialog, already reported idle and ready, and loses a prompt typed then
    /// although herdr accepts it; the pending prompt waits `settle` from here
    /// (`deliver_pending_prompt`, `deliver_held_prompts`). Kept in memory
    /// only: an actor started later comes well after the redraw.
    dialog_answered: HashMap<i64, Instant>,
    /// task id -> how often its prompt was sent again, for a task whose
    /// agent this actor gave its prompt (dispatch or a pending delivery) and
    /// has not seen `working` since. An agent still idle a settle window
    /// later at the sequence the prompt went in at never took it:
    /// `confirm_pending_done` sends it again, up to `MAX_PROMPT_RESENDS`
    /// times, then marks the task `blocked`. `blocked` alone does not prove
    /// the prompt was taken: it may be a startup dialog the prompt went into
    /// (`prompt_in_dialog`, `prompt_on_screen`). Kept in memory only, like
    /// the activity flag was: after a restart such a task stays `running`.
    unseen_prompt: HashMap<i64, u8>,
    /// Tasks whose agent this actor last saw `idle` or `done` (`unknown`
    /// changes nothing). Decides what an exit means: an agent that ends
    /// between turns finished its task; see `Observed::PaneExited`.
    idle_agents: HashSet<i64>,
    /// Tasks whose pane was read for Claude's limit picker since their
    /// agent was last seen `blocked` (`answer_limit_picker`): once a
    /// blocked spell. Cleared when the agent is seen out of `blocked`.
    picker_read: HashSet<i64>,
    /// task id -> its short limits in a row (`RateRetry`): a 429 or 529
    /// past the agent's own retries, retried in its pane. Kept in memory
    /// only: after a restart the count starts over.
    rate_retries: HashMap<i64, RateRetry>,
    /// Has the actor connected successfully at least once (ever)?
    was_connected: bool,
    /// Consecutive connect-attempt failures since the last success. Only decides
    /// the cold-start case in `announce_lost`; once `was_connected` is true a
    /// single failure is enough.
    failures: u32,
    /// Set once `machine.lost` has been emitted for the outage in progress, so it
    /// is never repeated; cleared by `announce_connected`.
    lost_announced: bool,
    /// Orphaned agents from the last reconcile, as (agent name, pane id).
    /// See `MachineStatus::orphans`.
    orphans: Vec<(String, String)>,
    /// By pane id, when `auto_close_stopped` first found that pane's agent
    /// stopped (idle or done), and who was in the pane then (`Occupant`):
    /// the grace for `close_failed_after` counts from there, not from when
    /// the task failed, went stale or was first found orphaned. Reset
    /// (removed) the moment the same pane is found at work again, so an
    /// agent that keeps going past the grace is not closed the instant it
    /// finally stops, and started again when another agent holds the pane:
    /// herdr hands pane ids out again, and the grace is the agent's.
    stopped_since: HashMap<String, (Occupant, Instant)>,
    /// When the machine was last asked for its pastor version; see
    /// `refresh_pastor_version`.
    version_asked_at: Instant,
    /// When the machine was last asked for its room; see `refresh_room`.
    room_asked_at: Instant,
    /// Tasks `start_save` has a save prompt out for right now, shared with
    /// the detached future it hands back (`SavingGuard`), each with whether
    /// the actor has seen its agent working since the prompt. What such an
    /// agent does is the save turn, not the task's work: its status is not
    /// applied to the row (`saving_saw`), and `auto_close_done` skips it.
    /// The flag is for the save's wait, which polls and could miss a short
    /// turn. Removed on every exit from that future, however it ends.
    saving: Arc<Mutex<HashMap<i64, bool>>>,
}

/// The pane of `task` if it is live on `machine`: starting with a pane
/// already, running, blocked, or done with its pane still open (an agent
/// marked done too early can be told to finish). Anything else has no agent
/// to type into.
fn live_pane(task: &Task, machine: &str) -> Result<String, SendRefused> {
    let live = matches!(
        task.state,
        TaskState::Starting | TaskState::Running | TaskState::Blocked | TaskState::Done
    );
    match (&task.pane_id, task.machine.as_deref()) {
        (Some(pane), Some(m)) if live && m == machine => Ok(pane.clone()),
        _ => Err(SendRefused {
            code: "task_not_live",
            message: format!(
                "{} is {}; only a starting, running, blocked or done task with a pane takes input",
                task.display_id(),
                task.state
            ),
        }),
    }
}

/// How often `SaveWork` looks at the agent while it saves. Short, since the
/// rule it waits by needs to see the agent working at least once, and a
/// commit and push can take only a few seconds.
const SAVE_POLL: Duration = Duration::from_millis(500);

/// What is left of `deadline`, or `None` once it has passed: the one clock
/// `start_save` checks preflight, prompt delivery and the detached wait
/// against, so none of them can together run longer than the `timeout` a
/// save was asked to keep inside.
fn time_left(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    (now < deadline).then(|| deadline - now)
}

/// `start_save`'s refusal for a `deadline` that passed before the agent's
/// work was confirmed saved, whichever step of it ran out the clock.
fn save_timeout_err(display: &str, timeout: Duration) -> anyhow::Error {
    SendRefused {
        code: "save_timeout",
        message: format!(
            "the agent of {display} did not finish saving its work within {timeout:?}"
        ),
    }
    .into()
}

/// `err` from a step `start_save` bounded by what was left of `deadline`:
/// `save_timeout` once that deadline has passed, since a step cut short by
/// its own share of the budget is the save timing out, not a herdr that
/// stopped answering; `err` unchanged with the deadline still ahead, a
/// genuine failure within it (an outage, say) that the caller should still
/// see as one.
fn or_save_timeout(
    err: anyhow::Error,
    deadline: Instant,
    display: &str,
    timeout: Duration,
) -> anyhow::Error {
    if time_left(deadline).is_none() {
        save_timeout_err(display, timeout)
    } else {
        err
    }
}

/// Keeps a task's id in `saving` from the moment its save prompt is sent
/// until the future `start_save` hands back for it ends, however it ends:
/// success, a refusal, or an error. `auto_close_done` reads the set this
/// guards to leave such a task's row and pane alone, since the row still
/// reads `Done` the whole time its agent commits and pushes.
struct SavingGuard {
    id: i64,
    saving: Arc<Mutex<HashMap<i64, bool>>>,
}

impl SavingGuard {
    fn new(saving: Arc<Mutex<HashMap<i64, bool>>>, id: i64) -> Self {
        saving.lock().unwrap().insert(id, false);
        Self { id, saving }
    }
}

impl Drop for SavingGuard {
    fn drop(&mut self) {
        self.saving.lock().unwrap().remove(&self.id);
    }
}

impl SavingGuard {
    /// Whether the actor has seen the agent working since the save prompt.
    fn seen_working(&self) -> bool {
        self.saving
            .lock()
            .unwrap()
            .get(&self.id)
            .copied()
            .unwrap_or(false)
    }
}

/// The agent in `pane`, by a fresh `agent.list`.
async fn agent_in(
    connector: &dyn Connector,
    pane: &str,
    timeout: Duration,
) -> anyhow::Result<AgentInfo> {
    let agents = tokio::time::timeout(timeout, connector.agent_list())
        .await
        .map_err(|_| TimedOut("agent.list", timeout))??;
    agents
        .into_iter()
        .find(|a| a.pane_id == pane)
        .with_context(|| format!("no agent in pane {pane}"))
}

/// Who holds a pane at one look: the agent's name (`Some(None)` for an
/// agent herdr lists with no name), or `None` for a pane with no agent.
type Occupant = Option<Option<String>>;

/// How long `occupant` has been stopped in `pane`, by `clocks`, starting
/// the clock if it is not running. A pane's clock runs for the agent it was
/// started for: another agent in the pane (a reused pane, or `find_orphans`
/// swapping one orphan for another) starts it again, so no agent inherits
/// time an earlier one spent stopped there.
fn stopped_for(
    clocks: &mut HashMap<String, (Occupant, Instant)>,
    pane: &str,
    occupant: Occupant,
) -> Duration {
    let (who, since) = clocks
        .entry(pane.to_string())
        .or_insert_with(|| (occupant.clone(), Instant::now()));
    if *who != occupant {
        *who = occupant;
        *since = Instant::now();
    }
    since.elapsed()
}

/// Does `pane` hold an agent that is not `task`'s: herdr handed the pane
/// out again, and that agent's status and sequence say nothing about the
/// task. An empty pane is still the task's.
fn holds_other_agent(task: &Task, agents: &[AgentInfo], pane: &str) -> bool {
    let expected = task
        .agent_name
        .clone()
        .unwrap_or_else(|| Task::agent_name_for(task.id));
    agents
        .iter()
        .find(|a| a.pane_id == pane)
        .is_some_and(|a| a.name.as_deref() != Some(expected.as_str()))
}

fn occupant(agents: &[AgentInfo], pane: &str) -> Occupant {
    agents
        .iter()
        .find(|a| a.pane_id == pane)
        .map(|a| a.name.clone())
}

/// Is `pane`'s agent stopped (idle or done) by `agents`, or is the pane
/// empty.
fn stopped_in(agents: &[AgentInfo], pane: &str) -> bool {
    agents
        .iter()
        .find(|a| a.pane_id == pane)
        .is_none_or(|a| matches!(a.agent_status, AgentStatus::Idle | AgentStatus::Done))
}

/// Would `next_state` read `pane`'s agent, by `agents`, as `task` finishing.
fn completion_pending_in(agents: &[AgentInfo], task: &Task, pane: &str) -> bool {
    agents
        .iter()
        .find(|a| a.pane_id == pane)
        .is_some_and(|agent| next_state(task, &observed_from(agent)) == Some(TaskState::Done))
}

/// Agents named `t-<id>` that none of `owned` (the pane-owning tasks on the
/// machine, `Store::tasks_on_machine`) accounts for, as (agent name, pane id).
/// Shared by the actor's reconcile and the daemon-less `machine status`.
pub fn orphan_agents(agents: &[AgentInfo], owned: &[Task]) -> Vec<(String, String)> {
    let owned: std::collections::HashSet<i64> = owned.iter().map(|t| t.id).collect();
    agents
        .iter()
        .filter_map(|a| {
            let name = a.name.as_deref()?;
            let id = task_id_of_agent(name)?;
            (!owned.contains(&id)).then(|| (name.to_string(), a.pane_id.clone()))
        })
        .collect()
}

/// The task id in an agent name pastor gives (`t-<id>`), and nothing looser:
/// `parse_task_id` also takes a bare number, which a human could name an agent,
/// and `t-01`, which `task close` would look up as `t-1`. Only a name that
/// `Task::agent_name_for` gives back unchanged counts.
fn task_id_of_agent(name: &str) -> Option<i64> {
    let digits = name.strip_prefix("t-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: i64 = digits.parse().ok()?;
    (id > 0 && Task::agent_name_for(id) == name).then_some(id)
}

/// Who ran `pastor task done`: the task's own agent, from its pane (or
/// through the bridge), or anyone else. A task whose `summary` is
/// `require` refuses its agent's `task done` without a summary, never a
/// person's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndBy {
    Agent,
    Hand,
    /// The head, whose row the agent's `task done` reached: a pull
    /// machine's serve ends its own row as the head's is, with the head's
    /// summary (`shepherd::Puller::report`). The head took the summary,
    /// so none is asked for again, and the round reads as it does there,
    /// not as one a person ended.
    Head,
}

/// How an idle agent ended its turn, read from its pane before a task is
/// settled `done` (`Actor::pane_end`).
enum PaneEnd {
    Finished,
    /// Claude or agy printed `DONE` alone as its last line
    /// (`task::trailing_done`) without running `pastor task done`: the task
    /// ends as if it had, with this message as its summary.
    Said(String),
    Question(String),
    /// Not Claude, idle with no `pastor task done` (`Actor::not_ended`):
    /// the pane's last lines. herdr cannot tell such an agent's pause
    /// (agy thinking, waiting on a process of its own) from its end.
    NotEnded(String),
    ShellRunning,
    /// Claude stopped under a dialog pastor answers (its autocompact
    /// offer): the keys that answer it, pressed by the caller. The turn has
    /// not ended for sure, so the window starts over.
    Dialog(crate::config::Dialog, Vec<String>),
    /// Claude stopped under a dialog that shows still but that pastor will
    /// not press: it was answered for this task already (and the pane did
    /// not clear), or the option it picks is not there. The task is
    /// `blocked` for a person, not closed under it.
    DialogUp(String),
    /// It stopped on a usage limit (`limit::limit_in`): not done, it waits
    /// for the reset or moves to its next model (`Actor::wait_on_limit`).
    /// With the pane's text, for an agent it hands the task to.
    Limit(crate::limit::Limit, String),
}

/// The error of a task whose agent is not Claude and went idle without
/// `pastor task done` (`PaneEnd::NotEnded`), before the pane's last lines.
pub const IDLE_WITHOUT_TASK_DONE: &str = "idle without task done";

/// How many of the pane's last lines `PaneEnd::NotEnded` keeps.
const IDLE_TAIL_LINES: usize = 5;

/// Whether an idle agent that is not Claude has yet to end `task`: it was
/// asked to run `pastor task done` (its `summary` is not `off`) and has not.
/// herdr shows such an agent idle while it thinks or waits on a process of
/// its own as it does when it is finished, and it draws nothing pastor can
/// tell those apart by, so only its own word ends the task. One never asked
/// ends on the idle, as before.
fn not_ended(task: &Task) -> bool {
    !task.ended && task.spec.summary != crate::task::SummaryMode::Off
}

/// A task's short limits in a row (`Actor::rate_retries`).
struct RateRetry {
    /// The retries scheduled so far, the one due included.
    attempts: u32,
    /// When `RATE_RETRY_PROMPT` goes into its pane; `None` once sent.
    due: Option<Instant>,
}

/// Who asked for a close. `pastor task close` refuses what herdr refuses;
/// auto-close of a done task closes the pane anyway when the worktree cannot
/// go without force, and writes the row only while it is still `Done`.
/// `Stopped` is `auto_close_stopped` removing the worktree of a failed task
/// that asked for it (`DispatchSpec::removes_worktree`): it keeps what it
/// cannot remove as auto-close does, and the row stays failed, with no pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseBy {
    Command,
    AutoClose,
    Stopped,
}

impl CloseBy {
    /// Pastor's own close, not a person's: it keeps, with a note, what a
    /// `task close` would refuse.
    fn is_auto(self) -> bool {
        self != CloseBy::Command
    }
}

/// What became of a worktree task's checkout removed with no herdr
/// workspace of the task's left to remove it by
/// (`Actor::remove_checkout_left`).
enum Removal {
    Gone,
    /// Kept, with the note saying why.
    Kept(String),
    /// Another agent works in it, named.
    Busy(String),
    /// git could not tell, or no checkout was recorded.
    Unknown,
}

/// What the inner loop should do after a command: nothing, reopen the event
/// subscription (the tracked pane set changed), or drop everything and reconnect
/// (a request failed below the API, so the machine looks lost).
enum CommandOutcome {
    Nothing,
    Resubscribe,
    Reconnect,
}

/// A pending `events.subscribe` call, boxed so `poll_until_subscribed` can hold
/// it across `select!` iterations without keeping `&self` (or `&mut self`)
/// borrowed for as long as the call takes to answer.
type SubscribeFuture = Pin<Box<dyn Future<Output = anyhow::Result<EventStream>> + Send>>;

/// How `poll_until_subscribed` ended: a fresh subscription, a request that
/// failed below the API while polling (carries the reason, same as
/// `connect_failed`'s message), or the command channel closing — the handle
/// was dropped, so nothing is asking about this machine any more and the actor
/// should stop instead of spinning trying to reconnect it.
enum PollExit {
    Subscribed(Box<EventStream>),
    Reconnect(String),
    Shutdown,
}

impl Actor {
    /// The version of each pinned harness on the machine (`harness_pins`),
    /// or `None` when a probe got no answer, so the last reading stays.
    async fn ask_harness(&self) -> Option<Vec<crate::harness::HarnessStatus>> {
        let mut out = Vec::new();
        for (kind, pin) in &self.settings.harness_pins {
            // A pin on a kind whose manifest is gone or reads no version has
            // nothing to ask.
            let Some(harness) = self
                .settings
                .harnesses
                .get(kind)
                .filter(|m| m.version.is_some())
            else {
                continue;
            };
            let asked = tokio::time::timeout(
                self.settings.request_timeout,
                self.connector.harness_version(harness),
            )
            .await;
            match asked {
                Ok(Ok(v)) => out.push(crate::harness::HarnessStatus::new(kind, pin, v)),
                Ok(Err(err)) => {
                    tracing::warn!(machine = %self.name, %kind, %err, "could not ask for the harness version");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(machine = %self.name, %kind, "harness version check timed out");
                    return None;
                }
            }
        }
        Some(out)
    }

    /// The machine's pastor version, for `machine list` only; `None` when
    /// the question got no answer. The ping or reconcile before it just proved
    /// the machine reachable, so a failure here, even an ssh one, is logged
    /// rather than failing the connection: the next request finds out soon
    /// enough if the machine really went away.
    async fn ask_pastor_version(&self) -> Option<Option<String>> {
        match tokio::time::timeout(
            self.settings.request_timeout,
            self.connector.pastor_version(),
        )
        .await
        {
            Ok(Ok(v)) => Some(v),
            Ok(Err(err)) => {
                tracing::warn!(machine = %self.name, %err, "could not ask for the pastor version");
                None
            }
            Err(_) => {
                tracing::warn!(machine = %self.name, "pastor version check timed out");
                None
            }
        }
    }

    /// Read the machine's room (`Connector::room`) into the status: the
    /// numbers stamped with now when it answers; on a failure or a timeout
    /// the error, over the numbers last read and their time, so a reader
    /// sees how old they are. As with the version, a failure here never
    /// touches the channel: the ping or reconcile before it just proved the
    /// machine reachable, and a machine never reconnects over its room.
    async fn read_room(&mut self) {
        self.room_asked_at = Instant::now();
        let read = match tokio::time::timeout(self.settings.request_timeout, self.connector.room())
            .await
        {
            Ok(Ok(room)) => Ok(room),
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err(format!(
                "room probe timed out after {:?}",
                self.settings.request_timeout
            )),
        };
        if let Err(error) = &read {
            tracing::warn!(machine = %self.name, %error, "could not read the room");
        }
        let mut s = self.status.write().recover();
        match read {
            Ok(Some(numbers)) => {
                s.room = Some(Room {
                    asked_at: Some(Utc::now()),
                    error: None,
                    ..numbers
                });
            }
            // A machine that cannot say has no room, not an empty one.
            Ok(None) => s.room = None,
            Err(error) => s.room.get_or_insert_with(Room::default).error = Some(error),
        }
    }

    /// Read the room again once `room_every` has passed since the last
    /// read. Called after a reconcile that worked, connected or polling,
    /// like `refresh_pastor_version`.
    async fn refresh_room(&mut self) {
        if self.room_asked_at.elapsed() < self.settings.room_every {
            return;
        }
        self.read_room().await;
    }

    async fn run(mut self) {
        let mut backoff = self.settings.initial_backoff;
        loop {
            self.set_channel(ChannelState::Connecting, None);
            // Nothing is held open for requests: a ping is an ordinary call, on a
            // connection of its own, and it is what proves the machine reachable.
            let pong =
                match tokio::time::timeout(self.settings.request_timeout, self.connector.ping())
                    .await
                {
                    Ok(Ok(p)) => p,
                    Ok(Err(err)) => {
                        self.connect_failed(err.to_string(), &mut backoff).await;
                        continue;
                    }
                    // A ping that never answers is exactly as dead as one that errors: a
                    // herdr that accepted the connection and then stopped answering must
                    // not hang the actor (and with it `Daemon::dispatch_queued` and the
                    // accept loop) forever.
                    Err(_) => {
                        self.connect_failed(
                            format!("ping timed out after {:?}", self.settings.request_timeout),
                            &mut backoff,
                        )
                        .await;
                        continue;
                    }
                };
            let asked = self.ask_pastor_version().await;
            let harness = self.ask_harness().await;
            self.version_asked_at = Instant::now();
            {
                let mut s = self.status.write().recover();
                if let Some(h) = harness {
                    s.harness = h;
                }
                s.herdr_version = Some(pong.version.clone());
                // An ask that got no answer keeps the version last read and
                // its time, as the periodic refresh does: a blank here would
                // read as "no pastor there".
                if let Some(v) = asked {
                    s.pastor_version = v;
                    s.pastor_read_at = Some(Utc::now());
                }
                s.protocol = Some(pong.protocol);
            }
            self.read_room().await;
            if pong.protocol < MIN_HERDR_PROTOCOL {
                self.set_channel(
                    ChannelState::Incompatible,
                    Some(format!(
                        "herdr protocol {} is older than {MIN_HERDR_PROTOCOL}; update herdr on this machine",
                        pong.protocol
                    )),
                );
                self.drain_commands_while_down(self.settings.max_backoff)
                    .await;
                continue;
            }
            if let Err(err) = self.reconcile().await {
                if is_outage(&err) {
                    self.connect_failed(format!("reconcile: {err}"), &mut backoff)
                        .await;
                    continue;
                }
                tracing::warn!(machine = %self.name, %err, "reconcile failed; staying connected");
            }
            let mut events = match self.open_events().await {
                Ok(s) => s,
                Err(err) => {
                    // ping and reconcile just succeeded: requests answer, only
                    // the subscription is missing. That is a machine to poll,
                    // not one to declare lost.
                    tracing::warn!(machine = %self.name, %err, "events will not open; polling");
                    self.set_channel(ChannelState::Polling, Some(format!("events: {err}")));
                    self.announce_connected();
                    self.refresh_live();
                    match self.poll_until_subscribed().await {
                        PollExit::Subscribed(s) => *s,
                        PollExit::Shutdown => return,
                        PollExit::Reconnect(reason) => {
                            // A request failed while polling: the machine is gone
                            // after all. Fall through to the reconnect path.
                            self.set_channel(ChannelState::Reconnecting, Some(reason));
                            self.announce_lost();
                            self.drain_commands_while_down(backoff).await;
                            backoff = (backoff * 2).min(self.settings.max_backoff);
                            continue;
                        }
                    }
                }
            };
            // Connected. `backoff` is deliberately left alone here: a flappy
            // connection that immediately breaks again must keep backing off, not
            // busy-spin at `initial_backoff` every time a connect attempt happens
            // to briefly succeed. It resets only once the connection proves itself
            // stable (below) or after a clean, command-driven resubscribe.
            self.set_channel(ChannelState::Connected, None);
            self.announce_connected();
            self.refresh_live();

            let connected_at = Instant::now();
            let mut settle_tick =
                tokio::time::interval(Duration::from_millis(50).max(self.settings.settle / 4));
            let mut reconcile_tick = tokio::time::interval(self.settings.reconcile_every);
            reconcile_tick.tick().await; // first tick fires immediately; we just reconciled
            // Auto-close runs after each reconcile. A grace shorter than
            // `reconcile_every` gets its own tick as well, or a 5s grace
            // would wait up to a whole reconcile.
            let close_every = [
                self.settings.close_done_after,
                self.settings.close_failed_after,
            ]
            .into_iter()
            .flatten()
            .min()
            .filter(|after| *after < self.settings.reconcile_every)
            .map(|after| after.max(Duration::from_millis(50)));
            let mut close_tick =
                tokio::time::interval(close_every.unwrap_or(self.settings.reconcile_every));
            close_tick.tick().await; // first tick fires immediately; we just reconciled
            // Only while the last reconcile went through: a machine whose
            // events will not open is not one to start closing panes on.
            let mut reconciled = true;
            loop {
                tokio::select! {
                    cmd = next_command(&mut self.rx, &mut self.now_rx) => {
                        let Some(cmd) = cmd else { return };
                        match self.handle_command(cmd).await {
                            CommandOutcome::Nothing => {}
                            CommandOutcome::Resubscribe => {
                                match self.open_events().await {
                                    Ok(s) => {
                                        events = s;
                                        // A fresh subscribe just succeeded: proof of health.
                                        backoff = self.settings.initial_backoff;
                                    }
                                    Err(err) => match self.poll_after_resubscribe_failed(err).await {
                                        PollExit::Subscribed(s) => events = *s,
                                        PollExit::Shutdown => return,
                                        PollExit::Reconnect(_) => break,
                                    },
                                }
                            }
                            CommandOutcome::Reconnect => {
                                tracing::warn!(machine = %self.name, "a request failed at the transport level; reconnecting");
                                break;
                            }
                        }
                    }
                    ev = events.next() => match ev {
                        Ok(ev) => {
                            if let Err(err) = self.handle_event(&ev).await {
                                if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "event handling failed"); break; }
                                tracing::warn!(machine = %self.name, %err, "event handling failed; staying connected");
                            }
                        }
                        Err(err) => { tracing::warn!(machine = %self.name, %err, "event stream ended"); break; }
                    },
                    _ = settle_tick.tick() => {
                        if let Err(err) = self.deliver_held_prompts().await {
                            if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "held prompt delivery failed"); break; }
                            tracing::warn!(machine = %self.name, %err, "held prompt delivery failed; staying connected");
                        }
                        if let Err(err) = self.send_rate_retries().await {
                            if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "short limit retry failed"); break; }
                            tracing::warn!(machine = %self.name, %err, "short limit retry failed; staying connected");
                        }
                        if let Err(err) = self.confirm_pending_done().await {
                            if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "settle check failed"); break; }
                            tracing::warn!(machine = %self.name, %err, "settle check failed; staying connected");
                        }
                    }
                    _ = reconcile_tick.tick() => {
                        reconciled = match self.reconcile().await {
                            Ok(false) => true,
                            Ok(true) => {
                                match self.open_events().await {
                                    Ok(s) => events = s,
                                    Err(err) => match self.poll_after_resubscribe_failed(err).await {
                                        PollExit::Subscribed(s) => events = *s,
                                        PollExit::Shutdown => return,
                                        PollExit::Reconnect(_) => break,
                                    },
                                }
                                true
                            }
                            Err(err) => {
                                if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "reconcile failed"); break; }
                                tracing::warn!(machine = %self.name, %err, "reconcile failed; staying connected");
                                false
                            }
                        };
                        // Only here, connected: the poll loop reconciles too, but
                        // it does not close panes.
                        if reconciled && let Err(err) = self.auto_close().await {
                            if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "auto-close failed"); break; }
                            tracing::warn!(machine = %self.name, %err, "auto-close failed; staying connected");
                        }
                        if reconciled {
                            self.refresh_pastor_version().await;
                            self.refresh_room().await;
                        }
                    }
                    _ = close_tick.tick(), if reconciled && close_every.is_some() => {
                        if let Err(err) = self.auto_close().await {
                            if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "auto-close failed"); break; }
                            tracing::warn!(machine = %self.name, %err, "auto-close failed; staying connected");
                        }
                    }
                }
            }
            self.set_channel(ChannelState::Reconnecting, Some("connection lost".into()));
            if connected_at.elapsed() >= self.settings.max_backoff {
                // Lived long enough to count as a real recovery, not a flap: start
                // the next outage's backoff from the floor again.
                backoff = self.settings.initial_backoff;
            }
            self.announce_lost();
            self.drain_commands_while_down(backoff).await;
            backoff = (backoff * 2).min(self.settings.max_backoff);
        }
    }

    /// A resubscribe from the connected loop failed. Only an outage ends the
    /// connection. Anything else (herdr refusing a subscription to a pane
    /// that closed since the reconcile that adopted it, a store error while
    /// building the list) leaves the machine answering, so it polls until a
    /// subscribe opens, like a subscription that will not open at connect.
    async fn poll_after_resubscribe_failed(&mut self, err: anyhow::Error) -> PollExit {
        if is_outage(&err) {
            tracing::warn!(machine = %self.name, %err, "resubscribe failed");
            return PollExit::Reconnect(format!("events: {err}"));
        }
        tracing::warn!(machine = %self.name, %err, "resubscribe refused; polling");
        self.set_channel(ChannelState::Polling, Some(format!("events: {err}")));
        self.refresh_live();
        let exit = self.poll_until_subscribed().await;
        if matches!(exit, PollExit::Subscribed(_)) {
            self.set_channel(ChannelState::Connected, None);
            self.refresh_live();
        }
        exit
    }

    /// Ask for the pastor version again once `version_every` has passed since
    /// the last ask. Called after a reconcile that worked, connected or
    /// polling, so an upgrade shows without a reconnect. A probe that got no
    /// answer keeps the version last read rather than blanking it until the
    /// next probe.
    async fn refresh_pastor_version(&mut self) {
        if self.version_asked_at.elapsed() < self.settings.version_every {
            return;
        }
        self.version_asked_at = Instant::now();
        let harness = self.ask_harness().await;
        if let Some(v) = self.ask_pastor_version().await {
            let mut s = self.status.write().recover();
            s.pastor_version = v;
            s.pastor_read_at = Some(Utc::now());
        }
        if let Some(h) = harness {
            self.status.write().recover().harness = h;
        }
    }

    async fn connect_failed(&mut self, message: String, backoff: &mut Duration) {
        self.failures += 1;
        tracing::warn!(machine = %self.name, %message, "connect failed");
        self.set_channel(ChannelState::Reconnecting, Some(message));
        self.announce_lost();
        self.drain_commands_while_down(*backoff).await;
        *backoff = (*backoff * 2).min(self.settings.max_backoff);
    }

    /// Emits `machine.lost` once per outage: immediately if the machine was
    /// connected before, otherwise only after two consecutive failures from a
    /// cold start. Stays quiet for the rest of the outage; `announce_connected`
    /// clears the flag on recovery.
    fn announce_lost(&mut self) {
        if self.lost_announced {
            return;
        }
        if self.was_connected || self.failures >= 2 {
            self.emit("machine.lost", None);
            self.lost_announced = true;
        }
    }

    /// Emits `machine.connected` only when recovering from an announced outage,
    /// never on the very first connect.
    fn announce_connected(&mut self) {
        if self.lost_announced {
            self.emit("machine.connected", None);
        }
        self.lost_announced = false;
        self.was_connected = true;
        self.failures = 0;
    }

    /// While down, answer commands with an error instead of leaving callers hanging.
    async fn drain_commands_while_down(&mut self, wait: Duration) {
        let deadline = tokio::time::sleep(wait);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return,
                cmd = next_command(&mut self.rx, &mut self.now_rx) => match cmd {
                    None => return,
                    Some(MachineCommand::Dispatch { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::Read { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::Close { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::Send { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::End { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::Pause { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::SaveWork { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::Resume { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::AutoTrust { reply }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::Sweep { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                },
            }
        }
    }

    fn set_channel(&self, channel: ChannelState, error: Option<String>) {
        let mut s = self.status.write().recover();
        s.channel = channel;
        s.error = error;
    }

    fn refresh_live(&self) {
        match self.store.tasks_on_machine(&self.name) {
            Ok(v) => {
                // An orphan holds a pane and an agent just like a task does;
                // leaving it out would let the picker over-dispatch.
                let mut s = self.status.write().recover();
                count_live(&mut s, &v);
                s.live += self.orphans.len();
                s.orphans = self.orphans.iter().map(|(name, _)| name.clone()).collect();
            }
            Err(err) => {
                // A store error is not "zero live agents": that reads as idle
                // capacity and would let the picker over-dispatch. Leave the
                // previous count as-is and only log.
                tracing::error!(machine = %self.name, %err, "refresh_live: store error, keeping previous live count");
            }
        }
    }

    /// Task events carry the task's job, read from its row, so a consumer that
    /// only filters by job (hooks, `pastor events`) needs no store lookup of its
    /// own. A row that cannot be read leaves `job` empty rather than dropping
    /// the event.
    fn emit(&self, kind: &str, task_id: Option<i64>) {
        self.emit_with(kind, task_id, None);
    }

    /// `emit` with a `PastorEvent::detail`. A task that turns `done` or
    /// `failed` here ends a round with no summary from its agent, so the
    /// round keeps the pane's last lines (`Store::end_round`); `end_task`
    /// ends its own with what the agent said. On a machine cut off from the
    /// head the round is `no head` (`Store::end_round_unreached`) unless the
    /// task asked for no summary.
    fn emit_with(&self, kind: &str, task_id: Option<i64>, detail: Option<serde_json::Value>) {
        let summary = task_id.filter(|_| matches!(kind, "task.done" | "task.failed"));
        if let Some(id) = summary {
            self.read_usage(id);
        }
        let summary = summary.and_then(|id| match self.end_round(id) {
            Ok(summary) => Some(summary),
            Err(err) => {
                tracing::error!(machine = %self.name, %err, id, "save the task's summary");
                None
            }
        });
        self.send_event(kind, task_id, detail, summary);
    }

    /// `Store::end_round` with no summary, or `end_round_unreached` when the
    /// task's agent could not have sent one: this machine is cut off from
    /// the head and the task asked for a summary. A row that cannot be read
    /// counts as asking.
    fn end_round(&self, id: i64) -> anyhow::Result<crate::task::TaskSummary> {
        let asked = || {
            !matches!(
                self.store.get_task(id),
                Ok(Some(t)) if t.spec.summary == crate::task::SummaryMode::Off
            )
        };
        if self.settings.cut_off_from_head && asked() {
            self.store.end_round_unreached(id)
        } else {
            self.store.end_round(id, None)
        }
    }

    /// Read what task `id`'s Claude session has used so far from its files
    /// on this machine (`Connector::claude_usage`) and keep it
    /// (`Store::set_usage`), when a round ends. In the background: the
    /// files can be large, and the actor must not wait on them. A task with
    /// no session (another kind of agent, or args that pick their own) has
    /// nothing to read; a read that fails keeps what was read before.
    fn read_usage(&self, id: i64) {
        let task = match self.store.get_task(id) {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(machine = %self.name, %err, id, "usage: cannot read task row");
                return;
            }
        };
        if self.settings.agents.kind(&task.spec.agent) != "claude" {
            return;
        }
        let Some(session) = task
            .spec
            .session_id
            .clone()
            .filter(|s| crate::task::is_session_id(s))
        else {
            return;
        };
        let config_dir =
            crate::usage::config_dir(&self.settings.agents, &task.spec.agent).map(String::from);
        let (connector, store, machine) = (
            self.connector.clone(),
            self.store.clone(),
            self.name.clone(),
        );
        tokio::spawn(async move {
            match connector
                .claude_usage(config_dir.as_deref(), &session)
                .await
            {
                Ok(Some(usage)) => {
                    if let Err(err) = store.set_usage(id, &usage) {
                        tracing::error!(%machine, %err, id, "save the task's usage");
                    }
                }
                Ok(None) => tracing::info!(%machine, id, "no usage found for the task's session"),
                Err(err) => tracing::warn!(%machine, %err, id, "read the task's usage"),
            }
        });
    }

    /// Send the event as it is, the task's job read from its row. `summary`
    /// is the round's summary at the moment the event is queued
    /// (`task.done`/`task.failed` only): it travels with the event rather
    /// than being read back from the store when the record is built, since
    /// by then the store may hold a later round's summary.
    fn send_event(
        &self,
        kind: &str,
        task_id: Option<i64>,
        detail: Option<serde_json::Value>,
        summary: Option<TaskSummary>,
    ) {
        let job = task_id.and_then(|id| match self.store.get_task(id) {
            Ok(t) => t.map(|t| t.job),
            Err(err) => {
                tracing::warn!(machine = %self.name, %err, id, "emit: cannot read task row");
                None
            }
        });
        tracing::info!(machine = %self.name, kind, ?task_id, ?job, "event");
        let _ = self.events.send(PastorEvent {
            detail,
            kind: kind.into(),
            task_id,
            machine: Some(self.name.clone()),
            job,
            summary,
        });
    }

    /// Builds the `events.subscribe` call (bounded by `request_timeout`) as a
    /// free-standing future that owns everything it needs, rather than
    /// borrowing `self`: `poll_until_subscribed` races it against commands and
    /// poll ticks in a `select!`, so it must be possible to hold one in flight
    /// across many loop iterations without tying up the actor for as long as a
    /// wedged herdr leaves it hanging. Building `subs` (a store read) is kept
    /// synchronous and done eagerly, before the future is returned.
    fn subscribe_future(&self) -> anyhow::Result<SubscribeFuture> {
        let mut subs = vec![
            subscription_lifecycle("pane.closed"),
            subscription_lifecycle("pane.exited"),
        ];
        for t in self.store.tasks_on_machine(&self.name)? {
            if let Some(p) = &t.pane_id {
                subs.push(subscription_agent_status(p));
            }
        }
        let connector = self.connector.clone();
        let timeout = self.settings.request_timeout;
        Ok(Box::pin(async move {
            Ok(tokio::time::timeout(timeout, connector.subscribe(subs))
                .await
                .map_err(|_| TimedOut("events.subscribe", timeout))??)
        }))
    }

    /// The one connection pastor keeps open: herdr dedicates it to events and
    /// never serves a request on it.
    ///
    /// The subscribe is bounded here rather than at each call site, so a herdr
    /// that accepts the connection and never acknowledges the subscription is
    /// treated like any other wedged request: both callers turn the error into a
    /// reconnect with backoff.
    async fn open_events(&self) -> Result<EventStream, anyhow::Error> {
        self.subscribe_future()?.await
    }

    /// The `Polling` loop: serve commands, reconcile every `poll_every`, retry
    /// the subscription on its own backoff (independent of `run`'s reconnect
    /// `backoff`; see the comment where `PollExit::Reconnect` is handled).
    ///
    /// The subscribe attempt itself is never awaited inline: it is started as
    /// a free-standing future (`subscribe_future`) and raced in the `select!`
    /// below alongside commands and poll ticks. Polling claims to accept
    /// dispatch (`ChannelState::Polling.accepts_dispatch()`), so a herdr that
    /// accepts `events.subscribe` and then never acknowledges it must not
    /// block a dispatch reply for up to `request_timeout` — that would stall
    /// every machine behind a fleet-wide dispatch lock, not just this one.
    async fn poll_until_subscribed(&mut self) -> PollExit {
        let mut poll = tokio::time::interval(self.settings.poll_every);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        poll.tick().await; // fires immediately; we reconciled a moment ago

        // The subscribe retry delay is local to this loop: it must not leak
        // into `run`'s `backoff`, which times the reconnect path and is left
        // exactly as `run` already manages it once polling ends.
        let mut retry_delay = self.settings.initial_backoff;
        let retry = tokio::time::sleep(retry_delay);
        tokio::pin!(retry);
        let mut subscribing: Option<SubscribeFuture> = None;

        loop {
            tokio::select! {
                cmd = next_command(&mut self.rx, &mut self.now_rx) => {
                    let Some(cmd) = cmd else { return PollExit::Shutdown };
                    match self.handle_command(cmd).await {
                        CommandOutcome::Nothing => {}
                        CommandOutcome::Resubscribe => {
                            // The tracked pane set changed (a dispatch added one).
                            // If nothing is in flight, the pending retry timer
                            // covers it: `subscribe_future` reads the store fresh
                            // whenever it is called, so the next attempt already
                            // includes the new pane. But an attempt already in
                            // flight was built from the pane set as it stood
                            // before this command; left alone, it would land the
                            // machine on `Connected` with a subscription missing
                            // the new pane, seen only at the next `reconcile_tick`
                            // (60s default). Drop it and start a fresh one now.
                            if subscribing.is_some() {
                                subscribing = None;
                                match self.subscribe_future() {
                                    Ok(fut) => subscribing = Some(fut),
                                    Err(err) => {
                                        retry_delay = (retry_delay * 2).min(self.settings.max_backoff);
                                        tracing::debug!(machine = %self.name, %err, next_in = ?retry_delay, "could not prepare the subscribe request");
                                        retry.as_mut().reset(Instant::now() + retry_delay);
                                    }
                                }
                            }
                        }
                        CommandOutcome::Reconnect => {
                            let reason = "a request failed at the transport level while polling".to_string();
                            tracing::warn!(machine = %self.name, "{reason}");
                            return PollExit::Reconnect(reason);
                        }
                    }
                }
                _ = poll.tick() => {
                    // Same rule as the connected loop: only an outage (transport
                    // failure or a request that never answered) means the machine
                    // is gone. A herdr API error or a store error is logged and
                    // the next tick tries again.
                    match self.reconcile().await {
                        // An attempt already in flight was built before the
                        // adoption and would miss the pane; start it over. With
                        // none in flight the next attempt reads the store fresh.
                        Ok(adopted) => {
                            if adopted && subscribing.is_some() {
                                subscribing = self.subscribe_future().ok();
                            }
                            self.refresh_pastor_version().await;
                            self.refresh_room().await;
                        }
                        Err(err) => {
                            tracing::warn!(machine = %self.name, %err, "poll reconcile failed");
                            if is_outage(&err) {
                                return PollExit::Reconnect(format!("poll reconcile failed: {err}"));
                            }
                        }
                    }
                    if let Err(err) = self.send_rate_retries().await {
                        tracing::warn!(machine = %self.name, %err, "short limit retry failed");
                        if is_outage(&err) {
                            return PollExit::Reconnect(format!("short limit retry failed: {err}"));
                        }
                    }
                    if let Err(err) = self.confirm_pending_done().await {
                        tracing::warn!(machine = %self.name, %err, "settle check failed");
                        if is_outage(&err) {
                            return PollExit::Reconnect(format!("settle check failed: {err}"));
                        }
                    }
                }
                // Only starts a new attempt once the previous one (if any) is
                // done: `subscribing` already holds one in flight otherwise.
                _ = &mut retry, if subscribing.is_none() => {
                    match self.subscribe_future() {
                        Ok(fut) => subscribing = Some(fut),
                        Err(err) => {
                            retry_delay = (retry_delay * 2).min(self.settings.max_backoff);
                            tracing::debug!(machine = %self.name, %err, next_in = ?retry_delay, "could not prepare the subscribe request");
                            retry.as_mut().reset(Instant::now() + retry_delay);
                        }
                    }
                }
                res = async { subscribing.as_mut().unwrap().await }, if subscribing.is_some() => {
                    subscribing = None;
                    match res {
                        Ok(s) => return PollExit::Subscribed(Box::new(s)),
                        Err(err) => {
                            retry_delay = (retry_delay * 2).min(self.settings.max_backoff);
                            tracing::debug!(machine = %self.name, %err, next_in = ?retry_delay, "subscribe still failing");
                            retry.as_mut().reset(Instant::now() + retry_delay);
                        }
                    }
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: MachineCommand) -> CommandOutcome {
        match cmd {
            MachineCommand::Dispatch { task_id, reply } => {
                let (result, mut dead, nows) = self.run_dispatch(task_id, false).await;
                dead |= nows.dead;
                if (matches!(&result, Ok(t) if t.state == TaskState::Blocked) || nows.blocked)
                    && let Err(err) = self.auto_answer().await
                {
                    tracing::warn!(machine = %self.name, "auto trust after dispatch: {err:#}");
                    dead |= is_outage(&err);
                }
                let changed = result.is_ok() || nows.changed;
                self.refresh_live();
                let _ = reply.send(result);
                match (dead, changed) {
                    (true, _) => CommandOutcome::Reconnect,
                    (false, true) => CommandOutcome::Resubscribe,
                    (false, false) => CommandOutcome::Nothing,
                }
            }
            MachineCommand::Close {
                task_id,
                remove_worktree,
                reply,
            } => {
                let (result, dead) = self
                    .run_close(task_id, remove_worktree, CloseBy::Command)
                    .await;
                let _ = reply.send(result);
                if dead {
                    CommandOutcome::Reconnect
                } else {
                    CommandOutcome::Nothing
                }
            }
            MachineCommand::Send {
                task_id,
                input,
                reply,
            } => {
                let (result, dead) = self.run_send(task_id, input).await;
                let _ = reply.send(result);
                if dead {
                    CommandOutcome::Reconnect
                } else {
                    CommandOutcome::Nothing
                }
            }
            MachineCommand::End {
                task_id,
                summary,
                by,
                reply,
            } => {
                let _ = reply.send(self.end_task(task_id, summary, by));
                CommandOutcome::Nothing
            }
            MachineCommand::Pause {
                task_id,
                for_task,
                reply,
            } => {
                let result = self.pause_task(task_id, for_task).await;
                let dead = matches!(&result, Err(err) if is_outage(err));
                self.refresh_live();
                let _ = reply.send(result);
                if dead {
                    CommandOutcome::Reconnect
                } else {
                    CommandOutcome::Nothing
                }
            }
            MachineCommand::SaveWork {
                task_id,
                timeout,
                reply,
            } => {
                // The wait runs beside the actor: an agent committing and
                // pushing takes minutes, and the actor has the rest of the
                // machine to serve meanwhile.
                match self.start_save(task_id, timeout).await {
                    Ok(wait) => {
                        tokio::spawn(async move {
                            let _ = reply.send(wait.await);
                        });
                        CommandOutcome::Nothing
                    }
                    Err(err) => {
                        let dead = is_outage(&err);
                        let _ = reply.send(Err(err));
                        if dead {
                            CommandOutcome::Reconnect
                        } else {
                            CommandOutcome::Nothing
                        }
                    }
                }
            }
            MachineCommand::Resume { task_id, reply } => {
                let (result, mut dead, nows) = self.run_dispatch(task_id, true).await;
                dead |= nows.dead;
                if (matches!(&result, Ok(t) if t.state == TaskState::Blocked) || nows.blocked)
                    && let Err(err) = self.auto_answer().await
                {
                    tracing::warn!(machine = %self.name, "auto trust after dispatch: {err:#}");
                    dead |= is_outage(&err);
                }
                let changed = result.is_ok() || nows.changed;
                self.refresh_live();
                let _ = reply.send(result);
                match (dead, changed) {
                    (true, _) => CommandOutcome::Reconnect,
                    (false, true) => CommandOutcome::Resubscribe,
                    (false, false) => CommandOutcome::Nothing,
                }
            }
            MachineCommand::Sweep { apply, reply } => {
                let result = self.run_sweep(apply).await;
                let dead = matches!(&result, Err(err) if is_outage(err));
                let _ = reply.send(result);
                if dead {
                    CommandOutcome::Reconnect
                } else {
                    CommandOutcome::Nothing
                }
            }
            MachineCommand::AutoTrust { reply } => {
                let result = self.auto_answer().await;
                let dead = matches!(&result, Err(err) if is_outage(err));
                let _ = reply.send(result);
                if dead {
                    CommandOutcome::Reconnect
                } else {
                    CommandOutcome::Nothing
                }
            }
            MachineCommand::Read {
                task_id,
                lines,
                reply,
            } => {
                let (result, dead) = match self.store.get_task(task_id) {
                    Ok(Some(t))
                        if t.machine.as_deref() == Some(&self.name) && t.state.occupies_pane() =>
                    {
                        let timeout = self.settings.request_timeout;
                        match tokio::time::timeout(
                            timeout,
                            self.connector
                                .agent_read(t.agent_name.as_deref().unwrap_or(""), lines),
                        )
                        .await
                        {
                            Ok(Ok(text)) => (Ok(text), false),
                            Ok(Err(err)) => {
                                let dead = err.is_transport();
                                (Err(err.into()), dead)
                            }
                            // A request that never answers within the bound is treated as a
                            // dead machine: the actor reconnects and backs off instead of
                            // hammering an apparently wedged herdr.
                            Err(_) => (
                                Err(anyhow::anyhow!("request timed out after {timeout:?}")),
                                true,
                            ),
                        }
                    }
                    Ok(_) => (
                        Err(anyhow::anyhow!(
                            "task {task_id} has no live agent on {}",
                            self.name
                        )),
                        false,
                    ),
                    Err(e) => (Err(e), false),
                };
                let _ = reply.send(result);
                if dead {
                    CommandOutcome::Reconnect
                } else {
                    CommandOutcome::Nothing
                }
            }
        }
    }

    /// `MachineCommand::Send`: the text, then the keys, into the task's pane,
    /// and a `task.input` event that records what was sent but not the text.
    /// Reports whether a request failed below the API, like `run_dispatch`.
    async fn run_send(&mut self, task_id: i64, input: SendInput) -> (anyhow::Result<Task>, bool) {
        let task = match self.store.get_task(task_id) {
            Ok(Some(t)) => t,
            Ok(None) => {
                let err = SendRefused {
                    code: "task_not_found",
                    message: format!("t-{task_id} not found"),
                };
                return (Err(err.into()), false);
            }
            Err(err) => return (Err(err), false),
        };
        let pane = match live_pane(&task, &self.name) {
            Ok(p) => p,
            Err(err) => return (Err(err.into()), false),
        };
        if input.is_empty() {
            let err = SendRefused {
                code: "nothing_to_send",
                message: "give text, --key, --trust or --pick".into(),
            };
            return (Err(err.into()), false);
        }
        if input.trust {
            return self.run_trust_send(task, pane).await;
        }
        let (text, keys, detail) = if let Some(want) = &input.pick {
            let timeout = self.settings.request_timeout;
            let target = task.agent_name.as_deref().unwrap_or(&pane);
            let screen =
                match tokio::time::timeout(timeout, self.connector.agent_read(target, 100)).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(err)) => {
                        let dead = err.is_transport();
                        return (Err(err.into()), dead);
                    }
                    Err(_) => return (Err(TimedOut("agent.read", timeout).into()), true),
                };
            let Some(found) = crate::picker::at_bottom(&screen) else {
                let tail: Vec<&str> = screen.lines().rev().take(5).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                let err = SendRefused {
                    code: "no_picker",
                    message: format!(
                        "{} shows no picker at the bottom of its pane:\n{}",
                        task.display_id(),
                        tail.join("\n")
                    ),
                };
                return (Err(err.into()), false);
            };
            let Some(keys) = found.picker.keys_to(want) else {
                let err = SendRefused {
                    code: "no_such_option",
                    message: format!(
                        "{} shows no option matching {want:?}; the picker has {}",
                        task.display_id(),
                        found.picker.listed()
                    ),
                };
                return (Err(err.into()), false);
            };
            let detail = serde_json::json!({"keys": keys, "pick": want});
            (None, keys, detail)
        } else {
            // Text that reopens a done task is new work, a new round: it
            // asks for a summary again, on the same line so no newline is
            // typed. A reply to an open task gets nothing: the prompt's
            // line is still in the agent's context.
            let text = input
                .text
                .clone()
                .map(|text| match task.spec.summary.ask_line() {
                    Some(line) if task.state == TaskState::Done => {
                        format!("{} {line}", text.trim_end())
                    }
                    _ => text,
                });
            (text, input.key_sequence(), input.detail())
        };
        // Text and its Enter go to a Claude session's inbox socket when it
        // takes them; keys of their own, a line left without Enter and a
        // pick stay keystrokes, as does whatever the socket does not take.
        let mut detail = detail;
        let by_socket = match &text {
            Some(text) if input.pick.is_none() && keys == ["Enter"] => {
                self.text_by_socket(&task, text).await
            }
            _ => false,
        };
        if by_socket {
            detail["socket"] = true.into();
        }
        let timeout = self.settings.request_timeout;
        let sent = tokio::time::timeout(timeout, async {
            if by_socket {
                return Ok(());
            }
            if let Some(text) = &text {
                self.connector.pane_send_text(&pane, text).await?;
            }
            if !keys.is_empty() {
                self.connector.pane_send_keys(&pane, &keys).await?;
            }
            Ok::<_, CallError>(())
        })
        .await;
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let dead = err.is_transport();
                return (Err(err.into()), dead);
            }
            Err(_) => return (Err(TimedOut("pane input", timeout).into()), true),
        }
        self.emit_with("task.input", Some(task.id), Some(detail));
        // Whatever the agent does next answers this input, not the prompt.
        self.unseen_prompt.remove(&task.id);
        if task.state == TaskState::Done {
            return (self.reopen(task), false);
        }
        // Input into a startup dialog, by whichever flag: the agent redraws,
        // and what follows may be another dialog, which herdr does not
        // report as a change, so the window's tick looks (`auto_answer`).
        if task.state == TaskState::Blocked && task.prompt_pending {
            self.dialog_answered.insert(task.id, Instant::now());
        }
        (Ok(task), false)
    }

    /// Send `text` to `task`'s Claude session through its inbox socket, when
    /// its agent delivers that way (`dispatch::Inbox`); whether the session
    /// took it. The `agent.list` read just before gates it as a pending
    /// prompt is gated (`prompt_by_socket`): an agent herdr lists blocked or
    /// still launching gets nothing, and the caller types the text, which a
    /// dialog may be waiting for. Any other answer is typed too.
    async fn text_by_socket(&self, task: &Task, text: &str) -> bool {
        let Some(inbox) = crate::dispatch::Inbox::of(&self.settings.agents, &task.spec) else {
            return false;
        };
        let name = task
            .agent_name
            .clone()
            .unwrap_or_else(|| Task::agent_name_for(task.id));
        let timeout = self.settings.request_timeout;
        let Ok(Ok(agents)) = tokio::time::timeout(timeout, self.connector.agent_list()).await
        else {
            return false;
        };
        let Some(agent) = agents.iter().find(|a| a.name.as_deref() == Some(&name)) else {
            return false;
        };
        if agent.agent_status == AgentStatus::Blocked || agent.launch_pending {
            return false;
        }
        let shown = task.display_id();
        let send = inbox.send(self.connector.as_ref(), &shown, text);
        matches!(tokio::time::timeout(timeout, send).await, Ok(true))
    }

    /// `MachineCommand::Send` with `--trust`: only the startup prompt (keys
    /// sent while the agent starts can land before the dialog, and would
    /// claim the task and save the repo; keys sent to a working agent go
    /// into its own UI), its agent's trust keys.
    ///
    /// Claude's dialog opens on "No, exit"; `Down` moves to "Yes, I trust
    /// this folder", which `Enter` then takes. A `Down` lost between pastor
    /// and the pane once left the dialog on "No, exit", so `Enter` quit
    /// Claude while pastor still marked the repo trusted. So when
    /// there is a marker to read the dialog by and more than one key, every
    /// key but the last goes first; the pane is read, and the last key goes
    /// only once the marker is the dialog's *selected* option, not merely
    /// shown (`trust_option_selected`, unlike `shows_trust_marker`, which
    /// `auto_answer` uses only to confirm the dialog is up before pressing
    /// every key in one go, since nothing there has pressed a key yet to
    /// lose). That stricter check only holds for the built-in Claude
    /// marker, known to be its correct option's own label
    /// (`Agents::trust_marker_is_option_text`); a configured one may name a
    /// heading instead, so it falls back to `shows_trust_marker`, the
    /// dialog merely being up, as every marker did before this check
    /// existed. A dialog that lands elsewhere is refused and saves no
    /// trust, the same as a send that fails outright. An agent with a
    /// single trust key, or none set as a marker, is answered in one press
    /// as before.
    async fn run_trust_send(&mut self, task: Task, pane: String) -> (anyhow::Result<Task>, bool) {
        if task.state != TaskState::Blocked || !task.prompt_pending {
            let err = SendRefused {
                code: "not_at_trust_prompt",
                message: format!(
                    "{} is {} and not waiting on its startup prompt; --trust answers only a task blocked while it starts",
                    task.display_id(),
                    task.state
                ),
            };
            return (Err(err.into()), false);
        }
        let Some(keys) = self.settings.agents.trust_keys(&task.spec.agent) else {
            let err = SendRefused {
                code: "no_trust_keys",
                message: format!(
                    "agent {} has no trust_keys; set [agents.{}] trust_keys in pastor.toml",
                    task.spec.agent, task.spec.agent
                ),
            };
            return (Err(err.into()), false);
        };
        let marker = self.settings.agents.trust_marker(&task.spec.agent);
        let marker_is_option = self
            .settings
            .agents
            .trust_marker_is_option_text(&task.spec.agent);
        let timeout = self.settings.request_timeout;
        // A built-in marker, the right option's own label, is checked even
        // before a lone key: agy's dialog takes Enter on whatever is selected.
        let (lead, confirm) = match (marker, keys.split_last()) {
            (Some(marker), Some((confirm, lead))) if !lead.is_empty() || marker_is_option => {
                (lead, Some((confirm.clone(), marker)))
            }
            _ => (keys.as_slice(), None),
        };
        if !lead.is_empty()
            && let Err((err, dead)) = self.send_keys_timed(&pane, lead, timeout).await
        {
            return (Err(err), dead);
        }
        if let Some((confirm, marker)) = confirm {
            let target = task.agent_name.as_deref().unwrap_or(&pane).to_string();
            let screen = match tokio::time::timeout(
                timeout,
                self.connector.agent_read(&target, 100),
            )
            .await
            {
                Ok(Ok(screen)) => screen,
                Ok(Err(err)) => {
                    let dead = err.is_transport();
                    return (Err(err.into()), dead);
                }
                Err(_) => return (Err(TimedOut("agent.read", timeout).into()), true),
            };
            let selected = if marker_is_option {
                crate::config::trust_option_selected(&screen, &marker)
            } else {
                crate::config::shows_trust_marker(&screen, &marker)
            };
            if !selected {
                let err = SendRefused {
                    code: "trust_not_selected",
                    message: format!(
                        "{} answered its folder-trust dialog with {lead:?}, but it is not on \"{marker}\"; \
                         not sending the key that confirms it, so it is not left to exit on another option",
                        task.display_id()
                    ),
                };
                return (Err(err.into()), false);
            }
            if let Err((err, dead)) = self
                .send_keys_timed(&pane, std::slice::from_ref(&confirm), timeout)
                .await
            {
                return (Err(err), dead);
            }
        }
        let detail = serde_json::json!({"keys": keys, "trust": true});
        self.emit_with("task.input", Some(task.id), Some(detail));
        self.unseen_prompt.remove(&task.id);
        self.dialog_answered.insert(task.id, Instant::now());
        // The prompt is answered: saved trust must not answer it again.
        if let Err(err) = self
            .store
            .claim_trust_sent(task.id, &keys)
            .and_then(|_| self.store.claim_answered(task.id, "trust", &keys))
        {
            return (Err(err), false);
        }
        if let Some(dir) = task.spec.trust_dir()
            && let Err(err) = self.store.trust_repo(&self.name, dir)
        {
            return (Err(err), false);
        }
        (Ok(task), false)
    }

    /// `keys` into `pane` within `timeout`: `Err((err, dead))` on failure,
    /// `dead` for a timeout or a transport failure, same as `run_send`'s own
    /// send.
    async fn send_keys_timed(
        &self,
        pane: &str,
        keys: &[String],
        timeout: Duration,
    ) -> Result<(), (anyhow::Error, bool)> {
        match tokio::time::timeout(timeout, self.connector.pane_send_keys(pane, keys)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                let dead = err.is_transport();
                Err((err.into(), dead))
            }
            Err(_) => Err((TimedOut("pane input", timeout).into(), true)),
        }
    }

    /// `MachineCommand::End`: the row alone, no herdr request. A task with a
    /// pane on this machine (starting, running, blocked, stale or done)
    /// becomes `done` and `ended`; its pane stays for `close_done_after`, as
    /// any done task's does, so the agent can finish the turn it said so in.
    ///
    /// A task whose `summary` is `require` refuses its own agent's `task
    /// done` without a summary (`SUMMARY_REQUIRED`) and leaves the row as
    /// it was, so the agent can run it again; a round it already ended with
    /// one keeps it. A person's is not refused; the round reads
    /// `ENDED_BY_HAND`.
    fn end_task(
        &mut self,
        task_id: i64,
        summary: Option<String>,
        by: EndBy,
    ) -> anyhow::Result<Task> {
        let Some(task) = self.store.get_task(task_id)? else {
            let err = SendRefused {
                code: "task_not_found",
                message: format!("t-{task_id} not found"),
            };
            return Err(err.into());
        };
        let live = |t: &Task| t.state.occupies_pane() && t.machine.as_deref() == Some(&self.name);
        if !live(&task) {
            let err = SendRefused {
                code: "task_not_live",
                message: format!(
                    "{} is {}; only a task with a pane on {} can end",
                    task.display_id(),
                    task.state,
                    self.name
                ),
            };
            return Err(err.into());
        }
        let summary = summary.filter(|s| !s.trim().is_empty());
        let required = task.spec.summary == crate::task::SummaryMode::Require;
        if required && summary.is_none() && by == EndBy::Agent && !task.ended {
            let err = SendRefused {
                code: crate::task::SUMMARY_REQUIRED,
                message: format!(
                    "{} needs a summary: pastor task done --current --summary '<summary>', a text in single quotes (a single quote inside it is written '\\'') whose first line is done, partial, blocked or nothing to do, then up to five short lines",
                    task.display_id()
                ),
            };
            return Err(err.into());
        }
        let by_hand = required && summary.is_none() && by == EndBy::Hand;
        let was_done = task.state == TaskState::Done;
        let written = write_task(&self.store, task, |t| {
            if !live(t) || t.ended {
                return false;
            }
            if t.state != TaskState::Done {
                t.state = TaskState::Done;
                t.finished_at = Some(Utc::now());
                t.activity_seen = false;
                t.prompt_pending = false;
                // Advice about a block the task has left.
                t.error = None;
            }
            t.ended = true;
            true
        })?;
        let ended_now = written
            .as_ref()
            .is_some_and(|t| !was_done && t.state == TaskState::Done);
        if ended_now {
            // The round ends here, with what the agent said or the pane.
            let round = if by_hand {
                self.store.end_round_by_hand(task_id)
            } else {
                self.store.end_round(task_id, summary.as_deref())
            };
            let round = match round {
                Ok(round) => Some(round),
                Err(err) => {
                    tracing::error!(machine = %self.name, %err, task_id, "save the task's summary");
                    None
                }
            };
            self.idle_agents.remove(&task_id);
            self.read_usage(task_id);
            self.send_event("task.done", Some(task_id), None, round);
        } else {
            if written.is_some() {
                self.idle_agents.remove(&task_id);
            }
            // Already done: what the agent says now is how that round ended.
            if let Some(summary) = &summary {
                self.store.replace_last_summary(task_id, summary)?;
            }
        }
        self.store
            .get_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("t-{task_id} not found"))
    }

    /// A done task that was just given more to do runs again. The baseline
    /// stays at the idle it was done at and `activity_seen` stays clear, as
    /// `apply` left them, so the agent's next turn is what marks it done.
    /// Its timeout counts from now, as a resume's does.
    /// A row that moved on meanwhile (closed, or picked up by an event) is
    /// left as it is.
    fn reopen(&mut self, task: Task) -> anyhow::Result<Task> {
        let id = task.id;
        let written = write_task(&self.store, task, |t| {
            if t.state != TaskState::Done {
                return false;
            }
            t.state = TaskState::Running;
            t.started_at = Some(Utc::now());
            t.finished_at = None;
            // The agent said it was finished; it has more to do now. An
            // ended task may have seen work after `task done` (the rest of
            // that turn), which must not count toward finishing the new one.
            if t.ended {
                t.activity_seen = false;
            }
            t.ended = false;
            true
        })?;
        let Some(t) = written else {
            return self
                .store
                .get_task(id)?
                .ok_or_else(|| anyhow::anyhow!("t-{id} not found"));
        };
        self.emit("task.running", Some(t.id));
        self.refresh_live();
        Ok(t)
    }

    /// `MachineCommand::Close`. herdr first, the row second: a herdr refusal
    /// (a dirty worktree, say) leaves the task as it was, so the command can
    /// be repeated. Reports whether a request failed below the API, like
    /// `run_dispatch`.
    async fn run_close(
        &mut self,
        task_id: i64,
        remove_worktree: bool,
        by: CloseBy,
    ) -> (anyhow::Result<Task>, bool) {
        self.leftovers = None;
        let closed = self.close_inner(task_id, remove_worktree, by).await;
        // Only a close that went through ends what the agent left running: a
        // refused one leaves the task and its agent as they were. A pane
        // closed before the refusal is the exception, the agent being gone.
        if let Some(mut left) = self.leftovers.take()
            && (closed.is_ok() || left.pane_closed)
        {
            if !left.checkout_gone {
                left.found.retain(|c| c.found != crate::reap::Found::Cwd);
            }
            self.end_leftovers(left).await;
        }
        match closed {
            Ok(t) => (Ok(t), false),
            Err(err) => {
                let dead = is_outage(&err);
                (Err(err), dead)
            }
        }
    }

    async fn close_inner(
        &mut self,
        task_id: i64,
        remove_worktree: bool,
        by: CloseBy,
    ) -> anyhow::Result<Task> {
        let row = self.store.get_task(task_id)?;
        let name = Task::agent_name_for(task_id);
        if by == CloseBy::AutoClose
            && !row.as_ref().is_some_and(|t| {
                t.state == TaskState::Done && t.machine.as_deref() == Some(&self.name)
            })
        {
            anyhow::bail!("task {name} is no longer done on {}", self.name);
        }
        if by == CloseBy::Stopped
            && !row.as_ref().is_some_and(|t| {
                t.state == TaskState::Failed && t.machine.as_deref() == Some(&self.name)
            })
        {
            anyhow::bail!("task {name} is no longer failed on {}", self.name);
        }
        // What auto-close leaves behind and says so on the row.
        let mut note = None;
        if let Some(t) = &row {
            if remove_worktree && !t.spec.worktree {
                anyhow::bail!("task {name} has no worktree to remove");
            }
            match t.machine.as_deref() {
                // Never dispatched: nothing on any machine to close.
                None => return self.finish_close(t.clone()),
                Some(m) if m != self.name => {
                    anyhow::bail!("task {name} is on machine {m}, not {}", self.name)
                }
                Some(_) => {}
            }
            // A paused or waiting task has no pane or agent; only its
            // checkout.
            if matches!(t.state, TaskState::Paused | TaskState::Waiting) {
                return self.close_paused(t.clone(), remove_worktree).await;
            }
            // A closed row with no workspace had its worktree removed (or
            // noted for manual cleanup) already: nothing is left to do.
            if remove_worktree && t.state == TaskState::Closed && t.workspace_id.is_none() {
                return Ok(t.clone());
            }
        }
        let timeout = self.settings.request_timeout;
        let agents = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        // The pane the row records, whatever its state: a failed task can
        // hold a pane with no agent in it (a dispatch that failed at
        // `agent.start`), which no name lookup would find. Not a closed
        // row's pane for an ordinary close: pastor closed that pane itself,
        // and herdr may have handed its id out again. A closed row's
        // *workspace* id is still good for `--remove-worktree`, though: a
        // plain close only closes the pane, not the checkout, so the id it
        // recorded can still be removed by even after the row is closed.
        // Otherwise (closed, no ids, no row) whatever agent still carries
        // the task's name.
        let mut recorded = row
            .as_ref()
            .filter(|t| t.state != TaskState::Closed || remove_worktree)
            .and_then(|t| t.pane_id.clone().map(|p| (p, t.workspace_id.clone())));
        // herdr hands a pane id out again once the pane closes, and its
        // workspace id with it. A recorded pane that now holds an agent under
        // another name is someone else's: closing it or removing its workspace
        // would take their work. An empty pane is still the task's (the failed
        // `agent.start` above). Auto-close leaves the row as it is; `task
        // close` looks for the task's agent by name instead, which is gone, so
        // it closes the row alone.
        let expected = row
            .as_ref()
            .and_then(|t| t.agent_name.clone())
            .unwrap_or_else(|| name.clone());
        if let Some((pane, _)) = &recorded
            && let Some(agent) = agents.iter().find(|a| &a.pane_id == pane)
            && agent.name.as_deref() != Some(expected.as_str())
        {
            let reused = PaneReused {
                pane: pane.clone(),
                agent: agent.name.clone(),
                expected,
            };
            if by.is_auto() {
                return Err(reused.into());
            }
            tracing::debug!(machine = %self.name, task = %name, %reused, "not closing a reused pane");
            recorded = None;
        }
        let target = recorded.or_else(|| {
            agents
                .iter()
                .find(|a| a.name.as_deref() == Some(name.as_str()))
                .map(|a| (a.pane_id.clone(), Some(a.workspace_id.clone())))
        });
        // The reconcile that found the task done listed a moment ago; the agent
        // may have gone back to work since. Closing it now would kill running
        // work: leave the row `Done` and let the status path move it on.
        if by.is_auto()
            && let Some((pane, _)) = &target
            && let Some(agent) = agents.iter().find(|a| &a.pane_id == pane)
            && !matches!(agent.agent_status, AgentStatus::Idle | AgentStatus::Done)
        {
            return Err(NotIdle(agent.agent_status).into());
        }
        // Idle here too, but a whole work cycle may have finished since the
        // row went `Done`, unseen by any reconcile in between (it never
        // showed `working`, only idle before and idle after): a moved
        // `completion_seq` (or `state_change_seq`, when herdr reports no
        // `completion_seq`) past the row's baseline means exactly that.
        // Closing now would destroy that unsettled completion; leave the row
        // for reconcile's settle window to catch up on instead.
        // An ended task needs neither check: its agent said it is finished,
        // and the turn it said so in moved the sequence on its own.
        if by == CloseBy::AutoClose
            && let Some((pane, _)) = &target
            && let Some(agent) = agents.iter().find(|a| &a.pane_id == pane)
            && let Some(t) = row.as_ref().filter(|t| !t.ended)
        {
            // Reading a missing baseline as 0 would make every such row look
            // moved, forever: reconcile writes no baseline for a row that is
            // already `Done`. Record what herdr reports now, in the same terms
            // as the comparison below, and compare from the next pass on.
            let Some(baseline) = t.last_completion_seq else {
                let seq = agent.completion_seq.unwrap_or(agent.state_change_seq);
                write_task(&self.store, t.clone(), |t| {
                    if t.state != TaskState::Done || t.last_completion_seq.is_some() {
                        return false;
                    }
                    t.last_completion_seq = Some(seq);
                    true
                })?;
                return Err(BaselineSeeded { seq }.into());
            };
            let moved = match agent.completion_seq {
                Some(seq) => seq > baseline,
                None => agent.state_change_seq > baseline,
            };
            if moved {
                return Err(SequenceMoved {
                    state_change_seq: agent.state_change_seq,
                    completion_seq: agent.completion_seq,
                    baseline,
                }
                .into());
            }
        }
        let Some((pane, workspace)) = target else {
            return match row {
                // No pane, so no workspace of the task's to remove the
                // checkout by: git removes it on the machine instead.
                Some(t) if remove_worktree => {
                    match self.remove_checkout_left(&t, &agents, None).await? {
                        Removal::Gone => self.finish_removal(t, by, None, true, None),
                        Removal::Kept(note) => self.finish_removal(t, by, None, false, Some(note)),
                        Removal::Busy(who) if by.is_auto() => {
                            let note = worktree_kept_note(&t, &name, &format!("{who} works in it"));
                            self.finish_removal(t, by, None, false, Some(note))
                        }
                        Removal::Busy(who) => anyhow::bail!(
                            "the worktree of {name} has another agent in it, {who}; close that first"
                        ),
                        Removal::Unknown => anyhow::bail!(manual_worktree_cleanup(&t.display_id())),
                    }
                }
                Some(t) if by == CloseBy::AutoClose => self.finish_auto_close(t, None),
                Some(t) => self.finish_close(t),
                None => anyhow::bail!("task {name} not found"),
            };
        };
        // The pane's agent is about to be closed below, taking with it any
        // conversation id it has not yet reported through a reconcile
        // (`capture_agent_session` otherwise only runs there): read it from
        // this same `agent.list` reply first, so a hook's report is not lost
        // to a close that outruns the next reconcile.
        let row = row.map(|t| match agents.iter().find(|a| a.pane_id == pane) {
            Some(agent) => self.capture_agent_session(t, agent),
            None => t,
        });
        let mut close_pane = !remove_worktree;
        let mut worktree_gone = false;
        let mut keep_worktree = false;
        // herdr removes a clean checkout even when its commits are on no
        // remote: a push that failed leaves the work only there. Auto-close
        // keeps it, as it keeps a dirty one. Unknown (no checkout recorded,
        // git could not tell) removes as before; herdr keeps the branch.
        // A task that asked for its worktree to go whatever it ends in
        // (`remove_worktree`) is not asked: its commits stay on the branch.
        if by == CloseBy::AutoClose
            && remove_worktree
            && let Some(checkout) = row
                .as_ref()
                .filter(|t| !t.spec.removes_worktree())
                .and_then(|t| t.spec.checkout.as_deref())
        {
            let unpushed =
                tokio::time::timeout(timeout, self.connector.unpushed_commits(&checkout.path))
                    .await
                    .map_err(|_| TimedOut("git rev-list", timeout))?
                    .map_err(CallError::from)
                    .with_context(|| {
                        format!("look for unpushed commits in the worktree of {name}")
                    })?;
            if unpushed == Some(true) {
                let t = row.as_ref().expect("auto-close has a row");
                note = Some(worktree_kept_note(t, &name, "commits on no remote"));
                keep_worktree = true;
                close_pane = true;
            }
        }
        // A worktree task placed in a workspace it did not make (`pastor`,
        // `pane:<label>`) records that workspace, which is never removed:
        // its checkout has no workspace until herdr opens one on it.
        let mut workspace = workspace;
        let mut reopened = None;
        let shared = row
            .as_ref()
            .filter(|t| t.spec.worktree && t.spec.place.is_shared());
        // The checkout stays while another agent works in it, before anything
        // is closed or opened. Under `place = "repo"` a task whose `--repo` is
        // this checkout (a fix round) joins the workspace showing it; placed
        // in `pastor` or `pane:<label>` it works there from a pane of the
        // shared workspace, where no workspace of the checkout lists it. So
        // other agents are looked for by the checkout's path too.
        if remove_worktree && !keep_worktree {
            let own = workspace.as_deref().filter(|_| shared.is_none());
            let checkout = row
                .as_ref()
                .and_then(|t| t.spec.checkout.as_deref())
                .map(|c| c.path.as_str());
            if let Some(who) = self
                .checkout_occupant(&agents, task_id, &pane, checkout, own)
                .await?
            {
                if !by.is_auto() {
                    anyhow::bail!(
                        "the worktree of {name} has another agent in it, {who}; close that first"
                    );
                }
                let t = row.as_ref().expect("auto-close has a row");
                note = Some(worktree_kept_note(t, &name, &format!("{who} works in it")));
                keep_worktree = true;
                close_pane = true;
            }
        }
        // Dispatch found a workspace already showing the checkout
        // (`Checkout::already_open`), whatever the task's place: a retry
        // placed `repo` or `own` joins the failed task's workspace or one
        // someone opened. pastor did not make it, and `worktree.remove`
        // takes the checkout by closing it: the checkout stays, noted for
        // removal by hand, and only the task's own pane goes.
        if remove_worktree
            && !keep_worktree
            && let Some(t) = row
                .as_ref()
                .filter(|t| t.spec.checkout.as_ref().is_some_and(|c| c.already_open))
        {
            let why = "a workspace pastor did not open showed it when the task started";
            note = Some(worktree_kept_note(t, &name, why));
            keep_worktree = true;
            close_pane = t.state != TaskState::Closed;
        }
        // What runs under the pane is listed before it goes, while herdr can
        // still say where its shell is, and for a checkout that goes too,
        // what works in it. Ended once the close went through (`run_close`).
        // Not a closed row's pane: pastor closed that one, and herdr may have
        // handed its id out again.
        if row.as_ref().is_none_or(|t| t.state != TaskState::Closed) {
            let cwd = row
                .as_ref()
                .filter(|_| remove_worktree && !keep_worktree)
                .and_then(|t| t.spec.checkout.as_ref())
                .filter(|c| !c.already_open)
                .map(|c| c.path.as_str());
            self.leftovers = Some(self.find_leftovers(row.as_ref(), &pane, &name, cwd).await);
        }
        // The task's pane goes first (not a closed row's: pastor closed that
        // one, and herdr may have handed its id out again), then the checkout
        // goes through a workspace `worktree.open` makes on it.
        if remove_worktree
            && !keep_worktree
            && let Some(t) = shared
        {
            if t.state != TaskState::Closed {
                self.close_pane_of(&pane, &name).await?;
                if let Some(left) = self.leftovers.as_mut() {
                    left.pane_closed = true;
                }
            }
            close_pane = false;
            match self.open_checkout(t, &name).await? {
                // One already showing the checkout is someone else's, and
                // `worktree.remove` takes the checkout by closing it: the
                // checkout stays, noted for removal by hand, as a dirty one.
                Some(created) if created.already_open => {
                    let why = format!(
                        "workspace {} shows it and pastor did not open it",
                        created.workspace.workspace_id
                    );
                    note = Some(worktree_kept_note(t, &name, &why));
                    keep_worktree = true;
                }
                Some(created) => {
                    workspace = Some(created.workspace.workspace_id);
                    reopened = Some(created.root_pane.pane_id);
                }
                // The checkout is gone already: nothing left to remove.
                None => {
                    worktree_gone = true;
                    self.checkout_went();
                    self.forget_workdir(row.as_ref());
                }
            }
        }
        if remove_worktree && !keep_worktree && !worktree_gone {
            let ws = workspace.with_context(|| format!("task {name} recorded no workspace"))?;
            // Closes the workspace, pane and agent with it: closing the pane
            // first would close the workspace and leave no id to remove by.
            let removed = tokio::time::timeout(timeout, self.connector.worktree_remove(&ws, false))
                .await
                .map_err(|_| TimedOut("worktree.remove", timeout))?;
            match removed {
                Ok(()) => {
                    worktree_gone = true;
                    self.checkout_went();
                    self.forget_workdir(row.as_ref());
                }
                // The recorded workspace is gone from herdr, pane and all:
                // closed some other way, or its checkout never existed.
                // The checkout may still be on disk, so git removes it on
                // the machine; when git cannot tell, auto-close takes it as
                // gone, and `task close` closes the row with the
                // manual-cleanup note rather than failing.
                Err(err) if err.code() == Some("workspace_not_found") => {
                    self.forget_orphan(&pane);
                    self.pending_done.remove(&task_id);
                    let removal = match row.as_ref() {
                        Some(t) => Some(self.remove_checkout_left(t, &agents, Some(&pane)).await?),
                        None => None,
                    };
                    // Only a checkout that is gone is searched.
                    if matches!(removal, Some(Removal::Gone)) {
                        self.checkout_went();
                    }
                    return match row {
                        Some(t) => match removal.expect("a row was looked at") {
                            Removal::Gone => self.finish_removal(t, by, Some(&pane), true, None),
                            Removal::Kept(note) => {
                                self.finish_removal(t, by, Some(&pane), true, Some(note))
                            }
                            Removal::Busy(who) if by.is_auto() => {
                                let why = format!("{who} works in it");
                                let note = worktree_kept_note(&t, &name, &why);
                                self.finish_removal(t, by, Some(&pane), true, Some(note))
                            }
                            Removal::Busy(who) => anyhow::bail!(
                                "the worktree of {name} has another agent in it, {who}; close that first"
                            ),
                            Removal::Unknown if by.is_auto() => {
                                self.finish_removal(t, by, Some(&pane), true, None)
                            }
                            Removal::Unknown => {
                                let note = manual_worktree_cleanup(&t.display_id());
                                self.finish_removal(t, by, Some(&pane), true, Some(note))
                            }
                        },
                        None => {
                            self.refresh_live();
                            Err(OrphanClosed {
                                agent: name,
                                machine: self.name.clone(),
                            }
                            .into())
                        }
                    };
                }
                // Never forced: an agent's uncommitted work is worth more than
                // a clean disk. The pane still goes; the checkout stays. Only
                // for an API refusal, where herdr actually answered with a
                // code: a protocol or decoding error is not herdr saying no,
                // so it falls to the branch below and is retried whole.
                Err(err) if by.is_auto() && err.code().is_some() => {
                    self.close_reopened(reopened.as_deref()).await;
                    let t = row.as_ref().expect("auto-close has a row");
                    let why = match err.code() {
                        Some("dirty_worktree_requires_force") => "uncommitted changes".to_string(),
                        _ => format!("herdr refused to remove it: {err}"),
                    };
                    note = Some(worktree_kept_note(t, &name, &why));
                    close_pane = shared.is_none();
                }
                Err(err) => {
                    self.close_reopened(reopened.as_deref()).await;
                    return Err(
                        anyhow::Error::from(err).context(format!("remove the worktree of {name}"))
                    );
                }
            }
        }
        if close_pane {
            self.close_pane_of(&pane, &name).await?;
        }
        self.forget_orphan(&pane);
        self.pending_done.remove(&task_id);
        match row {
            Some(t) if by == CloseBy::Stopped => {
                self.finish_stopped(t, Some(&pane), worktree_gone, note)
            }
            // The workspace is gone with the worktree. Clearing it is how
            // the row says so: prune keeps a worktree row that still
            // records one, since a plain close leaves the checkout on disk.
            Some(mut t) if by == CloseBy::AutoClose => {
                if worktree_gone {
                    t.workspace_id = None;
                }
                self.finish_auto_close(t, note)
            }
            // Kept, not removed: the row keeps its workspace, so another
            // `--remove-worktree` tries again once the checkout is free.
            Some(t) if remove_worktree && keep_worktree => {
                let note = note.expect("a kept worktree has a note");
                self.finish_close_with_note(t, note)
            }
            Some(mut t) if remove_worktree => {
                t.workspace_id = None;
                self.store.update_task(&mut t)?;
                self.finish_close(t)
            }
            Some(t) => self.finish_close(t),
            None => {
                self.refresh_live();
                Err(OrphanClosed {
                    agent: name,
                    machine: self.name.clone(),
                }
                .into())
            }
        }
    }

    /// List what runs under the shell of `pane`, and in `cwd` (the checkout
    /// that is about to go), before the pane closes (`reap`): closing a pane
    /// takes its shell, not a build the agent started under `flock` or
    /// `setsid`. It has to be asked while herdr can still say where the
    /// shell is, and nothing is signalled yet, since the close can still be
    /// refused: `end_leftovers` does that once it went through. Best effort:
    /// a pane herdr cannot describe, or a machine that cannot be reached,
    /// finds nothing and the close goes on.
    async fn find_leftovers(
        &self,
        task: Option<&Task>,
        pane: &str,
        name: &str,
        cwd: Option<&str>,
    ) -> Leftovers {
        let mut left = Leftovers {
            task_id: task.map(|t| t.id),
            pane: pane.to_string(),
            name: name.to_string(),
            found: vec![],
            pane_closed: false,
            checkout_gone: false,
        };
        let timeout = self.settings.request_timeout;
        let root = match tokio::time::timeout(timeout, self.connector.pane_shell_pid(pane)).await {
            Ok(Ok(pid)) => pid,
            Ok(Err(err)) => {
                tracing::debug!(machine = %self.name, task = %name, %err, "no process info for a pane about to close");
                None
            }
            Err(_) => None,
        };
        let spec = crate::reap::Spec {
            root,
            // Never the root of a disk, or a path that is not one.
            cwd: cwd
                .filter(|c| c.starts_with('/') && c.len() > 1)
                .map(str::to_string),
        };
        if spec.is_empty() {
            return left;
        }
        match tokio::time::timeout(timeout, self.connector.find_processes(&spec)).await {
            Ok(Ok(Some(found))) => left.found = found,
            Ok(Ok(None)) => {}
            Ok(Err(err)) => {
                tracing::warn!(machine = %self.name, task = %name, %err, "look for what a closing task left running");
            }
            Err(_) => {
                tracing::warn!(machine = %self.name, task = %name, "looking for what a closing task left running took too long");
            }
        }
        left
    }

    /// End what `find_leftovers` listed that still runs, now that the close
    /// went through: the agent and its tools went with the pane and are not
    /// among them. What was ended goes in `task.processes_killed`.
    async fn end_leftovers(&self, left: Leftovers) {
        if left.found.is_empty() {
            return;
        }
        let within = self.settings.request_timeout + crate::reap::GRACE + Duration::from_secs(5);
        let killed = match tokio::time::timeout(within, self.connector.end_processes(&left.found))
            .await
        {
            Ok(Ok(Some(killed))) => killed,
            Ok(Ok(None)) => return,
            Ok(Err(err)) => {
                tracing::warn!(machine = %self.name, task = %left.name, %err, "end what a closed task left running");
                return;
            }
            Err(_) => {
                tracing::warn!(machine = %self.name, task = %left.name, "ending what a closed task left running took too long");
                return;
            }
        };
        if killed.is_empty() {
            return;
        }
        let ended = killed.iter().filter(|k| k.ended).count();
        tracing::info!(machine = %self.name, task = %left.name, ended, listed = killed.len(), "ended what a closed task left running");
        self.emit_with(
            "task.processes_killed",
            left.task_id,
            Some(serde_json::json!({
                "pane": left.pane,
                "count": ended,
                "processes": killed,
            })),
        );
    }

    /// The checkout of the close under way is gone: what works in it is
    /// ended too.
    /// The workdir sets the head placed in `t`'s worktree leave with it
    /// (`workdir_task::Gate::forget`): its ledger and the repo's exclude
    /// lines for it would otherwise stay until a read finds it gone.
    fn forget_workdir(&self, t: Option<&Task>) {
        if let Some(c) = t.and_then(|t| t.spec.checkout.as_deref()) {
            self.forget_workdir_in(&c.path);
        }
    }

    fn forget_workdir_in(&self, dir: &str) {
        if let Some(gate) = &self.settings.workdir_gate.0 {
            gate.forget(&self.name, dir);
        }
    }

    fn checkout_went(&mut self) {
        if let Some(left) = self.leftovers.as_mut() {
            left.checkout_gone = true;
        }
    }

    /// `pane.close` of task `name`'s pane; one already gone is what closing
    /// wanted.
    async fn close_pane_of(&self, pane: &str, name: &str) -> anyhow::Result<()> {
        let timeout = self.settings.request_timeout;
        match tokio::time::timeout(timeout, self.connector.pane_close(pane))
            .await
            .map_err(|_| TimedOut("pane.close", timeout))?
        {
            Ok(()) => Ok(()),
            Err(err) if err.code() == Some("pane_not_found") => Ok(()),
            Err(err) => Err(anyhow::Error::from(err).context(format!("close the pane of {name}"))),
        }
    }

    /// Another agent at work in the checkout task `task_id` would remove,
    /// named (or its pane, when it has no name): one in `workspace` (the
    /// task's own on the checkout) or in any workspace showing `checkout`,
    /// or the agent of another open task on this machine (or a failed one
    /// that names an agent, which may still be running) whose checkout or
    /// `--repo` is `checkout`, wherever its pane is. `pane` is the task's own.
    /// With no checkout recorded, the path is the one herdr reports for
    /// `workspace`.
    async fn checkout_occupant(
        &self,
        agents: &[AgentInfo],
        task_id: i64,
        pane: &str,
        checkout: Option<&str>,
        workspace: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let timeout = self.settings.request_timeout;
        let workspaces = tokio::time::timeout(timeout, self.connector.workspace_list())
            .await
            .map_err(|_| TimedOut("workspace.list", timeout))??;
        let path = checkout.map(str::to_string).or_else(|| {
            workspaces
                .iter()
                .find(|w| Some(w.workspace_id.as_str()) == workspace)
                .and_then(|w| w.worktree.as_ref())
                .map(|c| c.checkout_path.clone())
        });
        let mut showing: Vec<&str> = workspace.into_iter().collect();
        let mut names = Vec::new();
        if let Some(path) = path.as_deref() {
            showing.extend(
                workspaces
                    .iter()
                    .filter(|w| {
                        w.worktree
                            .as_ref()
                            .is_some_and(|c| crate::dispatch::same_dir(&c.checkout_path, path))
                    })
                    .map(|w| w.workspace_id.as_str()),
            );
            for t in self.store.tasks_with_agents_on_machine(&self.name)? {
                if t.id == task_id {
                    continue;
                }
                let mut here = t
                    .spec
                    .checkout
                    .as_ref()
                    .is_some_and(|c| crate::dispatch::same_dir(&c.path, path));
                if !here && let Some(repo) = t.spec.repo.as_deref() {
                    // A `--repo` under `~` is the checkout only once expanded;
                    // one that cannot be is compared as written.
                    let repo = crate::dispatch::expand_home(
                        &*self.connector,
                        "repo",
                        repo,
                        Some(&self.name),
                    )
                    .await
                    .unwrap_or_else(|_| repo.to_string());
                    here = crate::dispatch::same_dir(&repo, path);
                }
                if here {
                    names.push(t.agent_name.unwrap_or_else(|| Task::agent_name_for(t.id)));
                }
            }
        }
        Ok(agents
            .iter()
            .find(|a| {
                a.pane_id != pane
                    && (showing.contains(&a.workspace_id.as_str())
                        || a.name.as_ref().is_some_and(|n| names.contains(n)))
            })
            .map(|a| a.name.clone().unwrap_or_else(|| a.pane_id.clone())))
    }

    /// A workspace showing the checkout of worktree task `t`, which dispatch
    /// placed in a workspace it did not make, so that `worktree.remove` can
    /// take it. `None` when herdr has no such checkout any more.
    async fn open_checkout(
        &self,
        t: &Task,
        name: &str,
    ) -> anyhow::Result<Option<crate::herdr::Created>> {
        let (Some(checkout), Some(repo)) = (t.spec.checkout.as_deref(), t.spec.repo.as_deref())
        else {
            anyhow::bail!(manual_worktree_cleanup(&t.display_id()));
        };
        let timeout = self.settings.request_timeout;
        let repo = crate::dispatch::expand_home(&*self.connector, "repo", repo, Some(&self.name))
            .await
            .map_err(|err| match err {
                crate::dispatch::DispatchError::Call(err) => anyhow::Error::from(err),
                other => anyhow::Error::from(other),
            })?;
        let opened = tokio::time::timeout(
            timeout,
            self.connector.worktree_open(&repo, &checkout.branch, name),
        )
        .await
        .map_err(|_| TimedOut("worktree.open", timeout))?;
        match opened {
            Ok(created) => Ok(Some(created)),
            Err(err) if err.code() == Some("worktree_not_found") => Ok(None),
            Err(err) => Err(anyhow::Error::from(err)
                .context(format!("open the worktree of {name} to remove it"))),
        }
    }

    /// Close the workspace `open_checkout` opened when its checkout stays:
    /// it was only a way to reach it. Best effort; a failure leaves a
    /// workspace on the checkout, which is where it was before the task.
    async fn close_reopened(&self, root: Option<&str>) {
        if let Some(root) = root
            && let Err(err) = self.close_pane_of(root, "a reopened worktree").await
        {
            tracing::debug!(machine = %self.name, %err, "could not close a reopened worktree");
        }
    }

    /// `finish_close`, but stamping `note` on the row first (it becomes the
    /// `NOTE` column the CLI shows): for a close that succeeds yet still has
    /// something the operator needs to know, like a checkout `--remove-worktree`
    /// could not reach through herdr.
    fn finish_close_with_note(&mut self, mut t: Task, note: String) -> anyhow::Result<Task> {
        t.error = Some(note);
        self.store.update_task(&mut t)?;
        self.finish_close(t)
    }

    /// `close_inner` for a paused task: its pane is gone already, so a plain
    /// close is the row alone. With `remove_worktree` its checkout, kept
    /// when it was paused, is opened in a workspace (`open_checkout`) and
    /// removed through it; a dirty one stays, the workspace goes again, and
    /// the task stays paused, as `task close --remove-worktree` leaves any
    /// task whose checkout herdr refuses.
    async fn close_paused(&mut self, t: Task, remove_worktree: bool) -> anyhow::Result<Task> {
        if !remove_worktree {
            return self.finish_close(t);
        }
        let name = t.display_id();
        let Some(created) = self.open_checkout(&t, &name).await? else {
            // Gone already: nothing left to remove.
            self.forget_workdir(Some(&t));
            return self.finish_close(t);
        };
        if created.already_open {
            let why = format!(
                "workspace {} shows it and pastor did not open it",
                created.workspace.workspace_id
            );
            let note = worktree_kept_note(&t, &name, &why);
            return self.finish_close_with_note(t, note);
        }
        let timeout = self.settings.request_timeout;
        let ws = created.workspace.workspace_id.clone();
        let removed = tokio::time::timeout(timeout, self.connector.worktree_remove(&ws, false))
            .await
            .map_err(|_| TimedOut("worktree.remove", timeout))?;
        match removed {
            Ok(()) => {
                self.forget_workdir(Some(&t));
                self.finish_close(t)
            }
            Err(err) => {
                self.close_reopened(Some(&created.root_pane.pane_id)).await;
                Err(anyhow::Error::from(err).context(format!("remove the worktree of {name}")))
            }
        }
    }

    fn finish_close(&mut self, t: Task) -> anyhow::Result<Task> {
        let was = t.state;
        let closed = self.store.close_task(t.id)?;
        if was != TaskState::Closed {
            self.emit("task.closed", Some(closed.id));
        }
        self.refresh_live();
        Ok(closed)
    }

    /// The row half of an auto-close: `t` is the row read as `Done` before
    /// herdr was asked, so the optimistic write lands only if nothing moved it
    /// since. `task.closed` is emitted here, once; `task close` goes through
    /// `finish_close` instead.
    fn finish_auto_close(&mut self, mut t: Task, note: Option<String>) -> anyhow::Result<Task> {
        t.state = TaskState::Closed;
        t.finished_at = t.finished_at.or_else(|| Some(Utc::now()));
        if note.is_some() {
            t.error = note;
        }
        self.store.update_task(&mut t)?;
        self.emit("task.closed", Some(t.id));
        self.refresh_live();
        Ok(t)
    }

    /// The row half of a close whose checkout `remove_checkout_left` dealt
    /// with, for whoever asked: `workspace_gone` clears the row's workspace,
    /// and `note` says what was kept.
    fn finish_removal(
        &mut self,
        mut t: Task,
        by: CloseBy,
        pane: Option<&str>,
        workspace_gone: bool,
        note: Option<String>,
    ) -> anyhow::Result<Task> {
        if workspace_gone || note.is_none() {
            t.workspace_id = None;
        }
        match (by, note) {
            (CloseBy::Stopped, note) => self.finish_stopped(t, pane, workspace_gone, note),
            (CloseBy::AutoClose, note) => self.finish_auto_close(t, note),
            (CloseBy::Command, Some(note)) => self.finish_close_with_note(t, note),
            (CloseBy::Command, None) => {
                self.store.update_task(&mut t)?;
                self.finish_close(t)
            }
        }
    }

    /// The row half of removing a failed task's worktree
    /// (`CloseBy::Stopped`): it stays failed, so `task retry` still takes
    /// it and makes a new worktree from its branch, and records no pane any
    /// more, nor a workspace once the checkout is gone. A note on a kept
    /// checkout follows the error the task failed with.
    fn finish_stopped(
        &mut self,
        t: Task,
        pane: Option<&str>,
        workspace_gone: bool,
        note: Option<String>,
    ) -> anyhow::Result<Task> {
        let written = write_task(&self.store, t.clone(), |t| {
            let ours = t.state == TaskState::Failed && t.pane_id.as_deref() == pane;
            if ours {
                t.pane_id = None;
                if workspace_gone {
                    t.workspace_id = None;
                }
                if let Some(note) = &note {
                    t.error = Some(match t.error.take() {
                        Some(e) => format!("{e}; {note}"),
                        None => note.clone(),
                    });
                }
            }
            ours
        })?;
        self.refresh_live();
        Ok(written.unwrap_or(t))
    }

    /// Remove the checkout of worktree task `t` when herdr has no
    /// workspace of the task's left on it (closed with its last pane, or
    /// gone some other way): `git worktree remove` on the machine, never
    /// forced, so uncommitted or untracked files keep it. The same checks
    /// as through herdr come first: a checkout another agent works in, one
    /// a workspace pastor did not open showed when the task started, or one
    /// a workspace still shows, stays. `pane` is the task's own, if any.
    async fn remove_checkout_left(
        &self,
        t: &Task,
        agents: &[AgentInfo],
        pane: Option<&str>,
    ) -> anyhow::Result<Removal> {
        let name = t.display_id();
        let Some(checkout) = t.spec.checkout.as_deref() else {
            return Ok(Removal::Unknown);
        };
        if let Some(kept) = self.checkout_held(t, agents, pane).await? {
            return Ok(kept);
        }
        let timeout = self.settings.request_timeout;
        let removed = tokio::time::timeout(timeout, self.connector.remove_checkout(&checkout.path))
            .await
            .map_err(|_| TimedOut("git worktree remove", timeout))?
            .map_err(CallError::from)
            .with_context(|| format!("remove the worktree of {name}"))?;
        Ok(match removed {
            Some(true) => {
                self.forget_workdir(Some(t));
                Removal::Gone
            }
            Some(false) => Removal::Kept(worktree_kept_note(
                t,
                &name,
                "uncommitted or untracked files",
            )),
            None => Removal::Unknown,
        })
    }

    /// What keeps the checkout of `t` from being removed that is not in the
    /// checkout itself: a workspace pastor did not open showed it when the
    /// task started, another agent works in it, or a workspace still shows
    /// it. None when nothing does (or no checkout is recorded).
    async fn checkout_held(
        &self,
        t: &Task,
        agents: &[AgentInfo],
        pane: Option<&str>,
    ) -> anyhow::Result<Option<Removal>> {
        let name = t.display_id();
        let Some(checkout) = t.spec.checkout.as_deref() else {
            return Ok(None);
        };
        if checkout.already_open {
            let why = "a workspace pastor did not open showed it when the task started";
            return Ok(Some(Removal::Kept(worktree_kept_note(t, &name, why))));
        }
        if let Some(who) = self
            .checkout_occupant(agents, t.id, pane.unwrap_or(""), Some(&checkout.path), None)
            .await?
        {
            return Ok(Some(Removal::Busy(who)));
        }
        let timeout = self.settings.request_timeout;
        let workspaces = tokio::time::timeout(timeout, self.connector.workspace_list())
            .await
            .map_err(|_| TimedOut("workspace.list", timeout))??;
        if let Some(w) = workspaces.iter().find(|w| {
            w.worktree
                .as_ref()
                .is_some_and(|c| crate::dispatch::same_dir(&c.checkout_path, &checkout.path))
        }) {
            let why = format!("workspace {} shows it", w.workspace_id);
            return Ok(Some(Removal::Kept(worktree_kept_note(t, &name, &why))));
        }
        Ok(None)
    }

    /// `pastor task sweep` on this machine: the worktrees pastor made for
    /// tasks that have ended (closed or failed) and that are still on
    /// disk, each either removed (with `apply`; or listed as `would_remove`
    /// without) or kept with the reason. A worktree is removed only when all
    /// of these hold, and each is asked of the machine itself:
    ///
    /// - the path is one a task row recorded for pastor's own checkout, looks
    ///   like a path (absolute, no `..`) and is not a repo a task works in;
    /// - no live task uses it: every row that records it, or has it as
    ///   `--repo`, has ended;
    /// - pastor opened it, no workspace shows it, no agent works in it
    ///   (`checkout_held`);
    /// - git finds no commit on no remote and no uncommitted or untracked
    ///   file (ignored files, a build's `target/`, go with the worktree);
    /// - and then `git worktree remove` is asked, never forced, which
    ///   refuses what the checks above missed.
    ///
    /// Anything that cannot be told is kept. The branch stays, and so do the
    /// task rows: a retry puts the checkout back.
    async fn run_sweep(&self, apply: bool) -> anyhow::Result<Vec<SweepEntry>> {
        let timeout = self.settings.request_timeout;
        let mut rows = self.store.list_tasks(&TaskFilter {
            machine: Some(self.name.clone()),
            ..Default::default()
        })?;
        // Oldest first, so a worktree's branch and its tasks read in order.
        rows.sort_by_key(|t| t.id);
        // A stale task's agent may still be at work (it turns failed once it
        // stops), so a stale task counts as live here.
        let ended = |t: &Task| matches!(t.state, TaskState::Closed | TaskState::Failed);
        // Worktrees by path, in the order the rows are first seen.
        let mut paths: Vec<String> = Vec::new();
        for t in &rows {
            if let Some(c) = t.spec.checkout.as_ref()
                && !paths.contains(&c.path)
            {
                paths.push(c.path.clone());
            }
        }
        let agents = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        let mut out = Vec::new();
        for path in paths {
            let here: Vec<&Task> = rows
                .iter()
                .filter(|t| {
                    t.spec
                        .checkout
                        .as_ref()
                        .is_some_and(|c| crate::dispatch::same_dir(&c.path, &path))
                })
                .collect();
            // Only what a task that ended left: a running task's own
            // worktree is not the sweep's business.
            if !here.iter().any(|t| ended(t)) {
                continue;
            }
            let first = here[0];
            let entry = |action, why: Option<String>| SweepEntry {
                machine: self.name.clone(),
                path: path.clone(),
                branch: first
                    .spec
                    .checkout
                    .as_ref()
                    .map(|c| c.branch.clone())
                    .unwrap_or_default(),
                tasks: here.iter().map(|t| t.display_id()).collect(),
                action,
                why,
            };
            let kept = |why: String| entry(SweepAction::Kept, Some(why));
            if !sweepable_path(&path) {
                out.push(kept(
                    "the recorded path does not look like a worktree".into(),
                ));
                continue;
            }
            // A live task: one that records the worktree, or works in it as
            // its `--repo` (a fix round).
            if let Some(live) = rows.iter().find(|t| {
                !ended(t)
                    && (t
                        .spec
                        .checkout
                        .as_ref()
                        .is_some_and(|c| crate::dispatch::same_dir(&c.path, &path))
                        || t.spec
                            .repo
                            .as_deref()
                            .is_some_and(|r| crate::dispatch::same_dir(r, &path)))
            }) {
                out.push(kept(format!(
                    "{} is live ({})",
                    live.display_id(),
                    live.state
                )));
                continue;
            }
            // A repo a task works in is not a worktree to sweep.
            if rows.iter().any(|t| {
                t.spec
                    .repo
                    .as_deref()
                    .is_some_and(|r| crate::dispatch::same_dir(r, &path))
            }) {
                out.push(kept("it is the repo a task works in".into()));
                continue;
            }
            if here
                .iter()
                .any(|t| t.spec.checkout.as_ref().is_some_and(|c| c.already_open))
            {
                out.push(kept(
                    "a workspace pastor did not open showed it when the task started".into(),
                ));
                continue;
            }
            let ask = async |what: &'static str, f: crate::herdr::transport::DirFuture<'_>| {
                tokio::time::timeout(timeout, f)
                    .await
                    .map_err(|_| TimedOut(what, timeout))?
                    .map_err(CallError::from)
                    .map_err(anyhow::Error::from)
            };
            match ask("test -e", self.connector.dir_exists(&path)).await? {
                Some(false) => continue,
                Some(true) => {}
                None => {
                    out.push(kept("could not tell whether it is still there".into()));
                    continue;
                }
            }
            match ask("git rev-list", self.connector.unpushed_commits(&path)).await? {
                Some(false) => {}
                Some(true) => {
                    out.push(kept("commits on no remote".into()));
                    continue;
                }
                None => {
                    out.push(kept("could not tell whether its commits are pushed".into()));
                    continue;
                }
            }
            match ask("git status", self.connector.checkout_dirty(&path)).await? {
                Some(false) => {}
                Some(true) => {
                    out.push(kept("uncommitted or untracked files".into()));
                    continue;
                }
                None => {
                    out.push(kept(
                        "could not tell whether it has uncommitted files".into(),
                    ));
                    continue;
                }
            }
            match self.checkout_held(first, &agents, None).await? {
                None => {}
                Some(Removal::Busy(who)) => {
                    out.push(kept(format!("{who} works in it")));
                    continue;
                }
                Some(Removal::Kept(_)) => {
                    out.push(kept("a workspace shows it".into()));
                    continue;
                }
                Some(_) => {
                    out.push(kept("something holds it".into()));
                    continue;
                }
            }
            if !apply {
                out.push(entry(SweepAction::WouldRemove, None));
                continue;
            }
            match ask("git worktree remove", self.connector.remove_checkout(&path)).await? {
                Some(true) => {
                    self.forget_workdir_in(&path);
                    out.push(entry(SweepAction::Removed, None));
                }
                Some(false) => out.push(kept("uncommitted or untracked files".into())),
                None => out.push(kept("git could not tell whether it removed it".into())),
            }
        }
        Ok(out)
    }

    /// Both auto-closes: done tasks (`auto_close_done`), then the panes of
    /// stopped agents no task is waiting on (`auto_close_stopped`).
    async fn auto_close(&mut self) -> anyhow::Result<()> {
        self.auto_close_done().await?;
        self.auto_close_stopped().await
    }

    /// Close the done tasks on this machine that finished `close_done_after`
    /// ago or more, through the same path as `pastor task close`, except
    /// those that keep their pane (`DispatchSpec::keeps_pane`). Failed and
    /// stale tasks lose only their panes (`auto_close_stopped`); blocked
    /// tasks are never closed on their own. A task herdr
    /// will not close is logged and tried again at the next check; only a
    /// failure below the API is returned, so the caller can reconnect.
    async fn auto_close_done(&mut self) -> anyhow::Result<()> {
        let Some(after) = self.settings.close_done_after else {
            return Ok(());
        };
        let grace = chrono::Duration::from_std(after).unwrap_or(chrono::Duration::MAX);
        let now = Utc::now();
        let tasks = self.store.tasks_on_machine(&self.name)?;
        // A done task with a save out for it is not idle in the sense this
        // check means: its agent is mid-commit or mid-push, and closing its
        // pane now would strand that work. `start_save` clears it here the
        // moment the save resolves, one way or another. Cloned out of the
        // lock, not held across it: a `std::sync::MutexGuard` is never
        // `Send`, and the loop below awaits.
        let saving: HashSet<i64> = self.saving.lock().unwrap().keys().copied().collect();
        let due: Vec<Task> = tasks
            .into_iter()
            .filter(|t| {
                t.state == TaskState::Done
                    && !t.spec.keeps_pane()
                    && !saving.contains(&t.id)
                    && now - t.finished_at.unwrap_or(t.updated_at) >= grace
            })
            .collect();
        for t in due {
            let (result, dead) = self
                .run_close(t.id, t.spec.worktree, CloseBy::AutoClose)
                .await;
            match result {
                Ok(closed) => {
                    tracing::info!(machine = %self.name, task = %closed.display_id(), ?after, note = ?closed.error, "auto-closed after close_done_after");
                }
                Err(err) if dead => return Err(err),
                Err(err) if err.is::<NotIdle>() => {
                    tracing::debug!(machine = %self.name, task = %t.display_id(), %err, "not auto-closed: the agent went back to work");
                }
                Err(err) if err.is::<PaneReused>() => {
                    tracing::debug!(machine = %self.name, task = %t.display_id(), %err, "not auto-closed: its pane holds another agent now");
                }
                Err(err) if err.is::<BaselineSeeded>() => {
                    tracing::debug!(machine = %self.name, task = %t.display_id(), %err, "not auto-closed yet: no completion baseline until now");
                }
                Err(err) if err.is::<SequenceMoved>() => {
                    tracing::debug!(machine = %self.name, task = %t.display_id(), %err, "not auto-closed: a new completion has not settled yet");
                }
                Err(err) => {
                    tracing::warn!(machine = %self.name, task = %t.display_id(), err = format!("{err:#}"), "auto-close failed; trying again at the next check");
                }
            }
        }
        Ok(())
    }

    /// Close the panes that stopped agents hold for no task pastor still
    /// waits on, once `close_failed_after` has passed since that pane's
    /// agent was first found stopped (idle or done; `stopped_since`): a
    /// `failed` or `stale` task's, and an orphan's (`find_orphans`). The
    /// grace resets the moment the same pane is next found at work, so an
    /// agent that keeps going past the check is not closed the instant it
    /// finally stops. A `stale` task's pane whose current idle observation
    /// `next_state` would still read as this task's completion is left
    /// alone instead: `confirm_pending_done`'s settle window owns deciding
    /// that, so a real completion is never raced into a failure; an idle
    /// observation that is not heading toward `Done` (an agent stuck idle
    /// with no completion coming) never sets that aside, so such a task is
    /// not kept forever. An agent herdr shows working or blocked is left
    /// alone. A pane that holds another agent now is told apart first,
    /// before its status or sequence is read: herdr handed the pane out
    /// again, so the task's own agent is gone and the replacement's status,
    /// working or idle, is not this task's work or completion. Such a task
    /// is repaired at once, with no grace: a stale one is failed and a
    /// failed one kept, both with their pane id cleared, and the
    /// replacement's pane is never touched.
    /// Unlike `auto_close_done`, the row is not closed and the worktree
    /// stays: a failed task stays failed and records no pane, so `pastor
    /// task retry` still takes it and can reopen its checkout. A stale task
    /// is failed first, before its pane goes, so the pane's `pane.closed`
    /// does not close the row, and so reconcile never reads it as a
    /// `Starting` task with no pane. A pane herdr will not close is logged
    /// and tried again at the next check; only a failure below the API is
    /// returned.
    /// Whether the checkout of `t` holds uncommitted or untracked files,
    /// or pastor cannot tell; false for a task with no checkout.
    async fn holds_unsaved_work(&self, t: &Task) -> anyhow::Result<bool> {
        let Some(checkout) = t.spec.checkout.as_deref() else {
            return Ok(false);
        };
        let timeout = self.settings.request_timeout;
        let dirty = tokio::time::timeout(timeout, self.connector.checkout_dirty(&checkout.path))
            .await
            .map_err(|_| TimedOut("git status", timeout))?
            .map_err(CallError::from)?;
        Ok(dirty != Some(false))
    }

    async fn auto_close_stopped(&mut self) -> anyhow::Result<()> {
        let Some(after) = self.settings.close_failed_after else {
            return Ok(());
        };
        // A task that keeps its pane is not a candidate at all: its pane
        // stays, and so does its row, stale or failed, until `task close`.
        // A failed task's agent also reads as an orphan, so its pane is
        // kept out of the orphans below too.
        let (kept, candidates): (Vec<Task>, Vec<Task>) = self
            .store
            .tasks_holding_panes(&self.name, &[TaskState::Failed, TaskState::Stale])?
            .into_iter()
            .partition(|t| t.spec.keeps_pane());
        let kept_panes: HashSet<String> = kept.into_iter().filter_map(|t| t.pane_id).collect();
        if candidates.is_empty() && self.orphans.is_empty() {
            return Ok(());
        }
        let timeout = self.settings.request_timeout;
        let agents = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        let stopped = |pane: &str| stopped_in(&agents, pane);
        // A pane whose agent is idle for a reason `next_state` would read as
        // this very task finishing: `confirm_pending_done` gets first say,
        // not this shorter grace.
        let pending_completion = |t: &Task, pane: &str| completion_pending_in(&agents, t, pane);
        // Start, or keep, each stopped pane's clock; a pane seen at work, or
        // whose idle is a pending completion, has its clock reset instead,
        // so a later stop (or a completion that turns out not to be one)
        // starts a fresh grace.
        // A pane that holds another agent now: herdr handed it out again,
        // so the task's own agent is gone, and the replacement's status,
        // working or idle at any sequence, says nothing about this task.
        // Checked before anything reads that status, and repaired at once:
        // there is no agent of this task left to give a grace to.
        let reused = |t: &Task, pane: &str| holds_other_agent(t, &agents, pane);
        let mut seen_panes: HashSet<String> = HashSet::new();
        // The clock of a reused pane is not marked seen here: it is the
        // earlier agent's, dropped below unless an orphan now holds the
        // pane, whose own clock starts then.
        let (reused_panes, candidates): (Vec<Task>, Vec<Task>) = candidates
            .into_iter()
            .partition(|t| reused(t, t.pane_id.as_deref().expect("filtered on a pane")));
        let mut due: Vec<Task> = vec![];
        for t in candidates {
            let pane = t.pane_id.clone().expect("filtered on a pane");
            seen_panes.insert(pane.clone());
            if !stopped(&pane) || pending_completion(&t, &pane) {
                self.stopped_since.remove(&pane);
            } else if stopped_for(&mut self.stopped_since, &pane, occupant(&agents, &pane)) >= after
            {
                due.push(t);
            }
        }
        let mut orphans_due: Vec<(String, String)> = vec![];
        for (agent, pane) in self.orphans.clone() {
            if kept_panes.contains(&pane) {
                continue;
            }
            seen_panes.insert(pane.clone());
            if !stopped(&pane) {
                self.stopped_since.remove(&pane);
            } else if stopped_for(&mut self.stopped_since, &pane, occupant(&agents, &pane)) >= after
            {
                orphans_due.push((agent, pane));
            }
        }
        self.stopped_since
            .retain(|pane, _| seen_panes.contains(pane));
        if due.is_empty() && orphans_due.is_empty() && reused_panes.is_empty() {
            return Ok(());
        }
        for t in reused_panes {
            let pane = t.pane_id.clone().expect("filtered on a pane");
            let name = t.display_id();
            tracing::debug!(machine = %self.name, task = %name, %pane, "not closing a reused pane");
            match t.state {
                TaskState::Failed => {
                    write_task(&self.store, t, |t| {
                        let ours = t.state == TaskState::Failed
                            && t.pane_id.as_deref() == Some(pane.as_str());
                        if ours {
                            t.pane_id = None;
                        }
                        ours
                    })?;
                }
                TaskState::Stale => {
                    let failed = write_task(&self.store, t.clone(), |t| {
                        let ours = t.state == TaskState::Stale
                            && t.pane_id.as_deref() == Some(pane.as_str());
                        if ours {
                            t.state = TaskState::Failed;
                            t.pane_id = None;
                            t.finished_at = t.finished_at.or_else(|| Some(Utc::now()));
                            t.error = Some(
                                "timed out and its agent's pane was reused; treated as stopped"
                                    .into(),
                            );
                        }
                        ours
                    })?;
                    if failed.is_some() {
                        self.pending_done.remove(&t.id);
                        self.emit("task.failed", Some(t.id));
                    }
                }
                _ => {}
            }
        }
        // Each `pane.close` can take up to `request_timeout`, and `agents`
        // was listed before the first: by the time a later pane's turn
        // comes, its agent may have gone back to work or herdr may have
        // handed the pane to another. So after every close the agents are
        // listed again, and a pane whose agent is no longer the stopped one
        // its clock was started for is kept, its clock dropped: the next
        // check starts a fresh grace, or repairs a reused task pane.
        let mut agents = agents;
        let mut listed = true;
        let mut closed = HashSet::new();
        for mut t in due {
            let pane = t.pane_id.clone().expect("filtered on a pane");
            let name = t.display_id();
            if !listed {
                agents = tokio::time::timeout(timeout, self.connector.agent_list())
                    .await
                    .map_err(|_| TimedOut("agent.list", timeout))??;
                listed = true;
            }
            if !self.still_stopped(&agents, &pane)
                || holds_other_agent(&t, &agents, &pane)
                || completion_pending_in(&agents, &t, &pane)
            {
                tracing::debug!(machine = %self.name, task = %name, "pane kept: its agent is at work, or another holds it, since it was found stopped");
                self.stopped_since.remove(&pane);
                continue;
            }
            if t.state == TaskState::Stale {
                let failed = write_task(&self.store, t.clone(), |t| {
                    let ours =
                        t.state == TaskState::Stale && t.pane_id.as_deref() == Some(pane.as_str());
                    if ours {
                        t.state = TaskState::Failed;
                        t.finished_at = t.finished_at.or_else(|| Some(Utc::now()));
                        t.error = Some(
                            "timed out and its agent stopped; its pane closed after close_failed_after"
                                .into(),
                        );
                    }
                    ours
                })?;
                let Some(failed) = failed else {
                    continue;
                };
                t = failed;
                self.pending_done.remove(&t.id);
                self.emit("task.failed", Some(t.id));
            }
            // Asked for its worktree to go whatever it ends in: removing it
            // through herdr closes the pane with it, and what cannot go is
            // kept with a note, as auto-close of a done task does.
            if t.spec.removes_worktree() {
                let (result, dead) = self.run_close(t.id, true, CloseBy::Stopped).await;
                listed = false;
                match result {
                    Ok(row) => {
                        tracing::info!(machine = %self.name, task = %name, ?after, note = ?row.error, "removed a stopped agent's worktree after close_failed_after");
                        closed.insert(pane);
                    }
                    Err(err) if dead => return Err(err),
                    Err(err) => {
                        tracing::warn!(machine = %self.name, task = %name, err = format!("{err:#}"), "removing a failed task's worktree failed; trying again at the next check");
                    }
                }
                continue;
            }
            let left = self.find_leftovers(Some(&t), &pane, &name, None).await;
            let close = self.close_pane_of(&pane, &name).await;
            listed = false;
            match close {
                Ok(()) => {}
                Err(err) if is_outage(&err) => return Err(err),
                Err(err) => {
                    tracing::warn!(machine = %self.name, task = %name, err = format!("{err:#}"), "closing a failed task's pane failed; trying again at the next check");
                    continue;
                }
            }
            self.end_leftovers(left).await;
            write_task(&self.store, t, |t| {
                let ours = t.pane_id.as_deref() == Some(pane.as_str());
                if ours {
                    t.pane_id = None;
                }
                ours
            })?;
            tracing::info!(machine = %self.name, task = %name, ?after, "closed a stopped agent's pane after close_failed_after");
            self.forget_orphan(&pane);
            closed.insert(pane);
        }
        // Looked for again, not taken from the last reconcile: a task may
        // have claimed the agent since.
        let owned = self.store.tasks_on_machine(&self.name)?;
        for (agent, pane) in orphans_due {
            if closed.contains(&pane) {
                continue;
            }
            if !listed {
                agents = tokio::time::timeout(timeout, self.connector.agent_list())
                    .await
                    .map_err(|_| TimedOut("agent.list", timeout))??;
                listed = true;
            }
            if !orphan_agents(&agents, &owned).contains(&(agent.clone(), pane.clone())) {
                continue;
            }
            if !self.still_stopped(&agents, &pane) {
                tracing::debug!(machine = %self.name, %agent, "orphan kept: it is at work, or another holds its pane, since it was found stopped");
                self.stopped_since.remove(&pane);
                continue;
            }
            let left = self.find_leftovers(None, &pane, &agent, None).await;
            let close = self.close_pane_of(&pane, &agent).await;
            listed = false;
            match close {
                Ok(()) => {}
                Err(err) if is_outage(&err) => return Err(err),
                Err(err) => {
                    tracing::warn!(machine = %self.name, %agent, err = format!("{err:#}"), "closing an orphan's pane failed; trying again at the next check");
                    continue;
                }
            }
            self.end_leftovers(left).await;
            tracing::info!(machine = %self.name, %agent, %pane, ?after, "closed an orphaned agent's pane after close_failed_after");
            self.forget_orphan(&pane);
        }
        self.refresh_live();
        Ok(())
    }

    /// Runs a dispatch and reports whether it failed below the API (a transport
    /// error, not a herdr error code): the caller must then treat the machine as
    /// lost and reconnect, not just resubscribe. The task's own outcome
    /// (including `Failed`, as `dispatch()` records it) is left to the store.
    /// A retry reopens its checkout (`dispatch::reopenable`) only while
    /// nobody works in it, and herdr lists an agent only in the workspace
    /// the agent's pane is in: a fix round placed in `pastor` works in the
    /// checkout from a pane of the shared workspace. So the agents are
    /// looked for by the checkout's path first, as removing it does
    /// (`checkout_occupant`); one there, or a look that fails, drops
    /// `reopen`, and the retry gets a new branch and worktree.
    async fn keep_occupied_checkout(&self, task: &mut Task) {
        let Some(path) = task.spec.reopen.as_ref().map(|r| r.path.clone()) else {
            return;
        };
        let timeout = self.settings.request_timeout;
        let look = async {
            let agents = self.connector.agent_list().await?;
            self.checkout_occupant(&agents, task.id, "", Some(&path), None)
                .await
        };
        let who = match tokio::time::timeout(timeout, look).await {
            Ok(Ok(who)) => who,
            Ok(Err(err)) => Some(format!("unknown ({err:#})")),
            Err(_) => Some(format!("unknown (no answer in {timeout:?})")),
        };
        if let Some(who) = who {
            tracing::info!(machine = %self.name, task = %task.display_id(), %path, %who, "not reopening a checkout in use");
            task.spec.reopen = None;
        }
    }

    /// `MachineCommand::Pause`. The agent is interrupted first (`esc`, so it
    /// ends its turn and saves the conversation as it stands, Claude's and
    /// agy's alike), then its pane closes, which ends the agent; a worktree
    /// stays on disk, since only `worktree.remove` deletes one. The row goes
    /// `paused` last, with no pane or workspace: the pane is gone, and the
    /// workspace closed with its last pane or is someone else's.
    async fn pause_task(&mut self, task_id: i64, for_task: i64) -> anyhow::Result<Task> {
        let task = self
            .store
            .get_task(task_id)?
            .with_context(|| format!("task t-{task_id} not found"))?;
        let refuse = |why: &str| PauseRefused {
            task: task.display_id(),
            why: why.to_string(),
        };
        if task.machine.as_deref() != Some(&self.name) {
            return Err(refuse(&format!("it is not on {}", self.name)).into());
        }
        let kind = self.settings.agents.kind(&task.spec.agent).to_string();
        if let Some(why) = crate::task::why_not_pausable(&task, &kind, Utc::now()) {
            return Err(refuse(why).into());
        }
        let name = task.display_id();
        self.interrupt_and_close(&task).await?;
        self.pending_done.remove(&task_id);
        let now = Utc::now();
        let paused = write_task(&self.store, task, |t| {
            if !t.state.occupies_pane() {
                return false;
            }
            t.state = TaskState::Paused;
            t.pane_id = None;
            t.workspace_id = None;
            t.prompt_pending = false;
            t.activity_seen = false;
            t.error = None;
            t.pause.paused_at = Some(now);
            t.pause.paused_for = Some(for_task);
            true
        })?
        .with_context(|| format!("{name} moved on while it was paused"))?;
        self.emit("task.paused", Some(task_id));
        Ok(paused)
    }

    /// Whether task `id` has a save out: then `status` is the save turn's,
    /// and is not applied to its row. A `working` is kept for the save's
    /// wait (`SavingGuard::seen_working`), and any window the task had
    /// open toward `done` is dropped, since the idle that would close it
    /// is the save's.
    fn saving_saw(&mut self, id: i64, status: AgentStatus) -> bool {
        {
            let mut saving = self.saving.lock().unwrap();
            let Some(seen) = saving.get_mut(&id) else {
                return false;
            };
            *seen |= status == AgentStatus::Working;
        }
        self.pending_done.remove(&id);
        true
    }

    /// The part of `MachineCommand::SaveWork` the actor does itself: check
    /// the task and its agent, and give it the prompt. What it returns waits
    /// for the agent to finish that prompt, off the actor.
    async fn start_save(
        &mut self,
        task_id: i64,
        timeout: Duration,
    ) -> anyhow::Result<Pin<Box<dyn Future<Output = anyhow::Result<Task>> + Send>>> {
        let deadline = Instant::now() + timeout;
        let Some(task) = self.store.get_task(task_id)? else {
            let err = SendRefused {
                code: "task_not_found",
                message: format!("t-{task_id} not found"),
            };
            return Err(err.into());
        };
        let pane = match (&task.pane_id, task.machine.as_deref()) {
            (Some(pane), Some(m)) if task.state.occupies_pane() && m == self.name => pane.clone(),
            _ => {
                let err = SendRefused {
                    code: "task_not_live",
                    message: format!(
                        "{} is {} with no pane on {}; there is no agent to save its work",
                        task.display_id(),
                        task.state,
                        self.name
                    ),
                };
                return Err(err.into());
            }
        };
        let display = task.display_id();
        let request_timeout = self.settings.request_timeout;
        let Some(budget) = time_left(deadline).map(|d| d.min(request_timeout)) else {
            return Err(save_timeout_err(&display, timeout));
        };
        let agent = agent_in(&*self.connector, &pane, budget)
            .await
            .map_err(|e| or_save_timeout(e, deadline, &display, timeout))?;
        if agent.agent_status == AgentStatus::Working {
            let err = SendRefused {
                code: "agent_busy",
                message: format!(
                    "the agent of {display} is working; save its work once it is idle"
                ),
            };
            return Err(err.into());
        }
        // Waiting for a person, or still launching: the prompt would land in
        // a dialog, or wait in a queue the save cannot see the end of.
        if agent.agent_status == AgentStatus::Blocked || agent.launch_pending {
            let err = SendRefused {
                code: "save_blocked",
                message: format!(
                    "the agent of {display} is waiting on a question; answer it, then save its work"
                ),
            };
            return Err(err.into());
        }
        let name = task
            .agent_name
            .clone()
            .unwrap_or_else(|| Task::agent_name_for(task.id));
        let prompt = crate::task::MOVE_SAVE_PROMPT;
        let Some(budget) = time_left(deadline).map(|d| d.min(request_timeout)) else {
            return Err(save_timeout_err(&display, timeout));
        };
        // Through the session's inbox socket when it takes it, as a task's
        // own prompt goes; typed otherwise. A socket send has no reply from
        // the agent, so the save counts from the `agent.list` read above,
        // with no work seen since, as `prompt_by_socket` does.
        let by_socket = match crate::dispatch::Inbox::of(&self.settings.agents, &task.spec) {
            Some(inbox) => matches!(
                tokio::time::timeout(
                    budget,
                    inbox.send(self.connector.as_ref(), &display, prompt)
                )
                .await,
                Ok(true)
            ),
            None => false,
        };
        let sent = if by_socket {
            agent
        } else {
            let Some(budget) = time_left(deadline).map(|d| d.min(request_timeout)) else {
                return Err(save_timeout_err(&display, timeout));
            };
            tokio::time::timeout(budget, self.connector.agent_prompt(&name, prompt))
                .await
                .map_err(|_| save_timeout_err(&display, timeout))??
        };
        // Tracked from the prompt landing, not from when the wait below
        // starts: a save is in flight the moment its prompt is sent, even
        // while this result sits between `handle_command` and the
        // `tokio::spawn` that awaits it. Dropped on every exit from the
        // future this hands back.
        let guard = SavingGuard::new(self.saving.clone(), task_id);
        let connector = self.connector.clone();
        let claude = self.settings.agents.kind(&task.spec.agent) == "claude";
        let store = self.store.clone();
        Ok(Box::pin(async move {
            // Counted from the prompt, as `deliver_pending_prompt` does: what
            // the agent did before it is not this prompt's work.
            let baseline = sent.state_change_seq;
            let mut active = sent.agent_status.is_activity();
            let mut last_seq = baseline;
            let saved: anyhow::Result<()> = async {
                loop {
                    let Some(sleep_for) = time_left(deadline) else {
                        return Err(save_timeout_err(&display, timeout));
                    };
                    tokio::time::sleep(SAVE_POLL.min(sleep_for)).await;
                    // The sleep can land right on the deadline: recheck before
                    // spending any more of it on a request, so a slow read never
                    // turns an overrun into a late success.
                    let Some(budget) = time_left(deadline).map(|d| d.min(request_timeout)) else {
                        return Err(save_timeout_err(&display, timeout));
                    };
                    let agent = agent_in(&*connector, &pane, budget)
                        .await
                        .map_err(|e| or_save_timeout(e, deadline, &display, timeout))?;
                    last_seq = last_seq.max(agent.state_change_seq);
                    // Stopped on a question mid-save (a permission prompt on
                    // the push, say): nobody is there to answer it in time.
                    if agent.agent_status == AgentStatus::Blocked {
                        let err = SendRefused {
                            code: "save_blocked",
                            message: format!(
                                "the agent of {display} stopped on a question instead of finishing the save"
                            ),
                        };
                        return Err(err.into());
                    }
                    if agent.agent_status == AgentStatus::Working {
                        active = true;
                        continue;
                    }
                    // A turn shorter than a poll is seen working only by the
                    // actor's events.
                    active |= guard.seen_working();
                    if !matches!(agent.agent_status, AgentStatus::Idle | AgentStatus::Done)
                        || !crate::task::completed_past(
                            baseline,
                            active,
                            Some(agent.state_change_seq),
                            agent.completion_seq,
                        )
                    {
                        continue;
                    }
                    // Idle past the prompt's own sequence is not proof by
                    // itself: herdr reads an agent idle while it waits on a
                    // shell of its own, or while agy is still thinking, the same
                    // way it reads one that is really done. Read the pane, as
                    // `pane_end` does for a real completion, before trusting it.
                    let Some(budget) = time_left(deadline).map(|d| d.min(request_timeout)) else {
                        return Err(save_timeout_err(&display, timeout));
                    };
                    let text = tokio::time::timeout(budget, connector.agent_read(&name, 100))
                        .await
                        .map_err(|_| save_timeout_err(&display, timeout))??;
                    if (claude && crate::task::background_shell_running(&text))
                        || (!claude && crate::task::tasks_running(&text))
                    {
                        // Still at work: keep polling at this sequence rather
                        // than call it done on an idle reading that is really
                        // the agent waiting on a shell or thinking.
                        continue;
                    }
                    let question = crate::task::trailing_question(&text)
                        .or_else(|| (!claude).then(|| crate::task::permission_question(&text))?);
                    if let Some(question) = question {
                        let err = SendRefused {
                            code: "save_blocked",
                            message: format!(
                                "the agent of {display} is asking instead of finishing the save: {question}"
                            ),
                        };
                        return Err(err.into());
                    }
                    if time_left(deadline).is_none() {
                        return Err(save_timeout_err(&display, timeout));
                    }
                    return Ok(());
                }
            }
            .await;
            // The save turn is not the task's work, however the save ended:
            // a later idle at its sequence completes nothing, so a move that
            // does not go ahead leaves the row as it was.
            if let Ok(Some(row)) = store.get_task(task_id) {
                let moved = write_task(&store, row, |t| {
                    let behind = t.last_completion_seq.is_none_or(|seq| seq < last_seq);
                    if behind {
                        t.last_completion_seq = Some(last_seq);
                    }
                    behind
                });
                if let Err(err) = moved {
                    let shown: &str = &display;
                    tracing::warn!(task = shown, %err, "record the save turn's sequence");
                }
            }
            drop(guard);
            saved?;
            store
                .get_task(task_id)?
                .with_context(|| format!("{display} vanished while it saved"))
        }))
    }

    /// Start task `task_id` (a paused one with `resume`), taking any
    /// `--now` dispatch that comes in meanwhile (`NowDispatch`) at once,
    /// beside it: a `--now` task never waits on another task's startup,
    /// which can run to `agent_ready_timeout`. Each of those is answered as
    /// soon as it has started; what they left (`NowsStarted`) is for the
    /// caller, like `task_id`'s own result.
    async fn run_dispatch(
        &mut self,
        task_id: i64,
        resume: bool,
    ) -> (anyhow::Result<Task>, bool, NowsStarted) {
        let mut now_rx = self.now_rx.take();
        let mut nows = NowsStarted::default();
        let (result, dead) = {
            let this = &*self;
            let main = this.start_task(task_id, resume);
            tokio::pin!(main);
            type Started<'a> = std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = (
                                (anyhow::Result<Task>, bool),
                                oneshot::Sender<anyhow::Result<Task>>,
                            ),
                        > + Send
                        + 'a,
                >,
            >;
            let mut starting: Vec<Started<'_>> = Vec::new();
            let mut answer = |((result, dead), reply): (
                (anyhow::Result<Task>, bool),
                oneshot::Sender<anyhow::Result<Task>>,
            )| {
                nows.note(&result, dead);
                this.refresh_live();
                let _ = reply.send(result);
            };
            // Urgent dispatches keep coming in until `task_id` and every one
            // already taken have started, not just `task_id`.
            let mut out = None;
            loop {
                if out.is_some() && starting.is_empty() {
                    break;
                }
                tokio::select! {
                    done = &mut main, if out.is_none() => out = Some(done),
                    Some(now) = recv_now(&mut now_rx) => {
                        starting.push(Box::pin(async move {
                            (this.start_task(now.task_id, false).await, now.reply)
                        }));
                    }
                    started = next_started(&mut starting), if !starting.is_empty() => answer(started),
                }
            }
            out.expect("the loop ends only once `main` is done")
        };
        self.now_rx = now_rx;
        for id in std::mem::take(&mut nows.uptake) {
            self.expect_uptake(id);
        }
        if let Ok(t) = &result
            && needs_uptake(t)
        {
            self.expect_uptake(t.id);
        }
        (result, dead, nows)
    }

    /// `run_dispatch` for one task, from the claim to its row and event.
    async fn start_task(&self, task_id: i64, resume: bool) -> (anyhow::Result<Task>, bool) {
        // The claim is the `Queued -> Starting` transition done as a conditional
        // UPDATE: a task another pass already took, or that finished meanwhile,
        // is simply not claimable. It also persists `machine` and `agent_name`
        // before the first herdr call, so a daemon crash mid-dispatch leaves a
        // row `reconcile` can adopt by agent name instead of one that still reads
        // `Queued` and gets dispatched twice.
        // A paused task is claimed only on the machine it was paused on.
        let claimed = if resume {
            self.store.claim_paused(task_id, &self.name)
        } else {
            self.store.claim_task(task_id, &self.name)
        };
        let mut task = match claimed {
            Ok(Some(t)) => t,
            Ok(None) => {
                let what = if resume { "paused here" } else { "queued" };
                return (
                    Err(anyhow::anyhow!(
                        "task t-{task_id} is not {what} (already claimed, finished, or unknown)"
                    )),
                    false,
                );
            }
            Err(e) => return (Err(e), false),
        };
        self.keep_occupied_checkout(&mut task).await;
        // Whether the agent goes back to a conversation it had: one that dies
        // at the start of a new one never got its prompt there.
        let resumes_session = resume && task.spec.session_id.is_some();
        // The machine's bundles first: `auto` applies a stale machine and
        // launches, `manual` launches with what is there, and a failed apply
        // stops the task in both. It has its own time, not the dispatch's.
        if let Some(gate) = &self.settings.bundle_gate.0
            && let Err(why) = gate.before_launch(&self.name).await
        {
            let message = format!("{}: {why}", crate::bundle::keep::BUNDLE_STALE);
            task.state = TaskState::Failed;
            task.error = Some(message.clone());
            task.finished_at = Some(Utc::now());
            if let Err(err) = self.store.update_task(&mut task) {
                return (Err(err), false);
            }
            self.emit("task.failed", Some(task.id));
            return (
                Err(anyhow::anyhow!("dispatch {}: {message}", task.display_id())),
                false,
            );
        }
        let timeout = self.settings.request_timeout;
        // A task's workdir set is placed inside the start, under its own
        // bound, which the start's own bound leaves room for: the herdr
        // requests keep the whole of `request_timeout`.
        let place_wait = timeout / 2;
        let start_wait = if task.spec.workdir.is_some() || !task.spec.workdir_bundles.is_empty() {
            timeout + place_wait
        } else {
            timeout
        };
        // Each start says afresh why it left a stored bundle out.
        for b in &mut task.spec.workdir_bundles {
            b.skipped = None;
        }
        // The trust pastor holds for this machine, for the folder of a task
        // it is about to start (`trust_write`).
        let trusted: Vec<String> = self
            .store
            .trusted_repos()
            .map(|all| {
                all.into_iter()
                    .filter(|t| t.machine == self.name)
                    .map(|t| t.repo)
                    .collect()
            })
            .unwrap_or_default();
        // A store that cannot be read holds the prompt for every row: at
        // worst a person answers a dialog again.
        let answered = self.answered_dialogs(&task).unwrap_or_default();
        let start = async {
            let (conn, agents) = (self.connector.as_ref(), &self.settings.agents);
            let head = self.settings.head_address.as_deref();
            let ready = self.settings.agent_ready_timeout;
            let provision = crate::dispatch::Provision {
                own: self.settings.own_policy.as_ref(),
                trusted: &trusted,
                trust_recheck: crate::trust_write::RECHECK,
                pins: Some(&self.settings.harness_pins),
                harnesses: Some(self.settings.harnesses.as_ref()),
                env: Some(&self.settings.env),
                answered: &answered,
                workdirs: Some(&self.settings.workdir_gate),
                workdir_wait: Some(place_wait),
            };
            if resume {
                crate::dispatch::resume_with(conn, &mut task, agents, head, ready, provision).await
            } else {
                dispatch_with(conn, &mut task, agents, head, ready, provision).await
            }
        };
        let outcome = match tokio::time::timeout(start_wait, start).await {
            Ok(outcome) => outcome,
            // `dispatch()` itself never got to resolve, so its own Failed-recording
            // never ran; do the same bookkeeping it would have done on an error, and
            // force a reconnect: a herdr that stops answering is a machine that is gone.
            Err(_) => {
                let message = format!("request timed out after {start_wait:?}");
                task.state = TaskState::Failed;
                task.error = Some(message.clone());
                task.finished_at = Some(Utc::now());
                if let Err(err) = self.store.update_task(&mut task) {
                    return (Err(err), true);
                }
                self.emit("task.failed", Some(task.id));
                return (
                    Err(anyhow::anyhow!("dispatch {}: {message}", task.display_id())),
                    true,
                );
            }
        };
        let dead = matches!(&outcome, Err(err) if err.is_transport());
        // An agent that died at its start on a usage limit: the task waits
        // for the reset, it has not failed.
        if let Err(crate::dispatch::DispatchError::Exited { tail, .. }) = &outcome
            && let Some(limit) = self.limit_in(&task, tail)
        {
            return (
                self.wait_at_start(task, &limit, !resumes_session, tail)
                    .await,
                false,
            );
        }
        if let Err(err) = self.store.update_task(&mut task) {
            return (Err(err), dead);
        }
        if let Some(why) = task.spec.fetch.warning.clone() {
            self.emit_with(
                "task.fetch_failed",
                Some(task.id),
                Some(serde_json::json!({"repo": task.spec.repo, "why": why})),
            );
        }
        if let Some(detail) = workdir_skipped(&task) {
            self.emit_with("workdir.skipped", Some(task.id), Some(detail));
        }
        match outcome {
            Ok(_) => {
                self.emit(&format!("task.{}", task.state), Some(task.id));
                (Ok(task), dead)
            }
            Err(err) => {
                self.emit("task.failed", Some(task.id));
                (
                    Err(anyhow::anyhow!("dispatch {}: {err}", task.display_id())),
                    dead,
                )
            }
        }
    }
}

/// The detail of `workdir.skipped` for a task whose start left its stored
/// workdir bundles out: their ids and why. `None` when it left none out.
pub(crate) fn workdir_skipped(task: &Task) -> Option<serde_json::Value> {
    let skipped: Vec<&crate::task::WorkdirBundle> = task
        .spec
        .workdir_bundles
        .iter()
        .filter(|b| b.skipped.is_some())
        .collect();
    let why = skipped.first()?.skipped.clone();
    let ids: Vec<&str> = skipped.iter().map(|b| b.id.as_str()).collect();
    Some(serde_json::json!({"ids": ids, "why": why}))
}

impl Actor {
    /// `wait_on_limit` for a task whose agent died at its start on `limit`:
    /// `task` is the row as the failed start left it, not yet written. Its
    /// pane is closed (there is no agent left to interrupt) and the row
    /// written `waiting`. `fresh_session` drops the session it started;
    /// `tail` is what its pane showed.
    async fn wait_at_start(
        &self,
        mut task: Task,
        limit: &crate::limit::Limit,
        fresh_session: bool,
        tail: &str,
    ) -> anyhow::Result<Task> {
        let row = self.keep_limit(&task, limit);
        if let Some(pane) = task.pane_id.clone()
            && let Err(err) = self.close_pane_of(&pane, &task.display_id()).await
        {
            tracing::warn!(machine = %self.name, task = %task.display_id(), err = %format!("{err:#}"), "close the pane of an agent that died on a usage limit");
        }
        let now = Utc::now();
        into_waiting(&mut task, &row, fresh_session, now);
        self.stop_on_limit(&mut task, &row, limit, tail, now);
        self.store.update_task(&mut task)?;
        self.emit_waiting(&task, &row, now);
        Ok(task)
    }
}

/// Should the actor watch that `task`'s agent takes its prompt
/// (`Actor::expect_uptake`)? A resume's prompt can be lost as a dispatch's can.
fn needs_uptake(task: &Task) -> bool {
    task.state == TaskState::Running && !task.activity_seen
}

/// What the `--now` dispatches `Actor::run_dispatch` took beside its own
/// task left for its caller.
#[derive(Default)]
struct NowsStarted {
    /// Tasks to watch take their prompt (`needs_uptake`).
    uptake: Vec<i64>,
    /// One started and is `blocked`: run `auto_trust`.
    blocked: bool,
    /// One started: its pane joins the subscription.
    changed: bool,
    /// One failed below the API.
    dead: bool,
}

impl NowsStarted {
    fn note(&mut self, result: &anyhow::Result<Task>, dead: bool) {
        if let Ok(t) = result {
            self.changed = true;
            self.blocked |= t.state == TaskState::Blocked;
            if needs_uptake(t) {
                self.uptake.push(t.id);
            }
        }
        self.dead |= dead;
    }
}

impl Actor {
    async fn handle_event(&mut self, ev: &crate::herdr::Event) -> anyhow::Result<()> {
        let Some(pane_id) = ev.pane_id() else {
            return Ok(());
        };
        let Ok(Some(task)) = self.store.find_by_pane(&self.name, pane_id) else {
            if ev.is_pane_closed() {
                self.release_failed_pane(pane_id)?;
            }
            if (ev.is_pane_closed() || ev.is_pane_exited()) && self.forget_orphan(pane_id) {
                self.refresh_live();
            }
            return Ok(());
        };
        let id = task.id;
        let observed = if ev.is_pane_closed() {
            Observed::PaneClosed
        } else if ev.is_pane_exited() {
            Observed::PaneExited {
                agent_idle: self.idle_agents.contains(&task.id),
            }
        } else if let Some(status) = ev.agent_status() {
            self.note_status(task.id, status, task.prompt_pending);
            // Subscription events carry no sequence; treat idle as a candidate and let
            // the settle check read the real one from agent.list.
            Observed::Status {
                status,
                state_change_seq: None,
                completion_seq: None,
            }
        } else {
            return Ok(());
        };
        if let Observed::Status { status, .. } = &observed
            && self.saving_saw(task.id, *status)
        {
            return Ok(());
        }
        if let Observed::Status { status, .. } = &observed
            && task.prompt_pending
            && self.deliver_pending_prompt(task.clone(), *status).await?
        {
            return Ok(());
        }
        if let Observed::Status {
            status: AgentStatus::Blocked,
            ..
        } = &observed
            && self.prompt_in_dialog(&task).await?
        {
            self.auto_answer().await?;
            return Ok(());
        }
        match &observed {
            Observed::Status {
                status: crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done,
                ..
            } => {
                // A fresh window, unless one is already open: an agent that
                // went back to work in between sent a working event, which
                // cleared the old one, so normally there is nothing here yet.
                // But `reconcile`'s own agent.list poll can also open this
                // task's window, with the real sequence already read off the
                // agent (`Some`, not this event's placeholder `None`); a
                // subscription event for the very idle that poll already
                // caught must not throw that reading away and restart the
                // window from scratch.
                self.pending_done
                    .entry(task.id)
                    .or_insert((None, Instant::now()));
            }
            _ => {
                self.pending_done.remove(&task.id);
                self.apply(task, &observed);
            }
        }
        if matches!(
            observed,
            Observed::Status {
                status: AgentStatus::Blocked,
                ..
            }
        ) {
            self.answer_limit_picker(id).await?;
            self.auto_answer().await?;
        }
        Ok(())
    }

    /// The startup dialogs pastor has answered for `task` (`Store::answered`).
    fn answered_dialogs(&self, task: &Task) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        for d in self.settings.agents.dialogs(&task.spec.agent) {
            if self.store.answered(task.id, &d.name)? {
                out.push(d.name);
            }
        }
        Ok(out)
    }

    /// Send the prompt the agent has not seen yet (see `Task::prompt_pending`)
    /// once it has left `blocked` or its launch. Returns whether it dealt with
    /// `status`; `false` leaves the observation to the usual state machine.
    ///
    /// Without this, a human who clears the agent's startup question sees the
    /// task flip to `running` while the agent sits idle, never having received
    /// the work it was started for.
    async fn deliver_pending_prompt(
        &mut self,
        task: Task,
        status: AgentStatus,
    ) -> anyhow::Result<bool> {
        if matches!(status, AgentStatus::Blocked | AgentStatus::Unknown) {
            return Ok(false);
        }
        match self.dialog_answered.get(&task.id) {
            Some(at) if at.elapsed() < self.settings.settle => {
                // Still redrawing after its trust dialog: the settle tick
                // sends it (`deliver_held_prompts`).
                return Ok(true);
            }
            Some(_) => {
                self.dialog_answered.remove(&task.id);
            }
            None if task.state == TaskState::Blocked => {
                // Out of its startup block with no answer from pastor: a
                // person answered it at the pane, just now. The agent redraws
                // as after any trust answer, so the prompt waits out `settle`.
                self.dialog_answered.insert(task.id, Instant::now());
                return Ok(true);
            }
            None => {}
        }
        let name = task
            .agent_name
            .clone()
            .unwrap_or_else(|| Task::agent_name_for(task.id));
        let timeout = self.settings.request_timeout;
        // Still at a startup question herdr reads idle (Codex's folder
        // trust, any of agy's): the prompt would answer it and be lost. The
        // next status or reconcile looks again (and answers a dialog pastor
        // knows), and waits out the redraw once it is gone.
        let answered = self.answered_dialogs(&task)?;
        let hold = self
            .settings
            .agents
            .startup_markers(&task.spec.agent, &answered);
        if !hold.is_empty() {
            match tokio::time::timeout(timeout, self.connector.agent_read(&name, 100))
                .await
                .map_err(|_| TimedOut("agent.read", timeout))?
            {
                Ok(screen) if crate::config::shows_startup_question(&screen, &hold) => {
                    self.dialog_answered.remove(&task.id);
                    return Ok(true);
                }
                Ok(_) => {}
                Err(err) if err.is_transport() => return Err(err.into()),
                Err(err) => {
                    tracing::warn!(machine = %self.name, task = %task.display_id(), %err, "read pane for a startup question");
                }
            }
        }
        let prompt = crate::task::pending_prompt(&task);
        let by_socket = match crate::dispatch::Inbox::of(&self.settings.agents, &task.spec) {
            Some(inbox) => self.prompt_by_socket(&inbox, &task, &name, &prompt).await?,
            None => BySocket::Type,
        };
        // The agent as the prompt went in, and whether it was at work by
        // then (see `dispatch::Prompted`).
        let result = match by_socket {
            BySocket::Wait => return Ok(true),
            BySocket::Sent(agent) => Ok((*agent, false)),
            BySocket::Type => {
                tokio::time::timeout(timeout, self.connector.agent_prompt(&name, &prompt))
                    .await
                    .map_err(|_| TimedOut("agent.prompt", timeout))?
                    .map(|agent| {
                        let activity = agent.agent_status.is_activity();
                        (agent, activity)
                    })
            }
        };
        let id = task.id;
        // The reply reports the agent's session as well as `agent.list` does.
        let session = result.as_ref().ok().and_then(|(agent, _)| {
            crate::task::reported_session(self.settings.agents.kind(&task.spec.agent), agent)
                .map(String::from)
        });
        // Only a row still waiting for this prompt, on a live pane, takes the
        // outcome. A row closed or failed meanwhile is left as it is.
        let still_pending = |t: &Task| t.prompt_pending && t.state.occupies_pane();
        let written = match result {
            Ok((agent, activity)) => {
                let written = write_task(&self.store, task, |t| {
                    if !still_pending(t) {
                        return false;
                    }
                    if t.state == TaskState::Blocked {
                        // Same as `apply`: a startup block cleared past the
                        // old deadline gets a fresh timeout, not one already
                        // spent waiting for a person.
                        t.started_at = Some(Utc::now());
                    }
                    t.prompt_pending = false;
                    t.state = TaskState::Running;
                    t.error = None;
                    t.finished_at = None;
                    // State changes before the agent had our prompt (its launch,
                    // its startup question) are not this task's work: count from here.
                    t.last_completion_seq = Some(agent.state_change_seq);
                    if let Some(session) = &session {
                        t.spec.session_id = Some(session.clone());
                    }
                    // Set on the fresh row too: a re-read row never carries it.
                    t.activity_seen = activity;
                    true
                })?;
                // `status` was what the prompt went in on, typically idle; the
                // reply is newer. Without it, an exit before the next event (or
                // any time while polling) would read as between turns.
                if written.is_some() {
                    self.note_status(id, agent.agent_status, false);
                }
                written
            }
            // Blocked again, or between states: the next status event or
            // reconcile tries again.
            Err(err) if matches!(err.code(), Some("agent_blocked" | "agent_not_ready")) => {
                return Ok(true);
            }
            Err(err) if err.is_transport() => return Err(err.into()),
            Err(err) => {
                let message = format!("could not send the pending prompt: {err}");
                let now = Utc::now();
                write_task(&self.store, task, |t| {
                    if !still_pending(t) {
                        return false;
                    }
                    t.state = TaskState::Failed;
                    t.error = Some(message.clone());
                    t.finished_at = Some(now);
                    t.activity_seen = false;
                    true
                })?
            }
        };
        self.pending_done.remove(&id);
        if written
            .as_ref()
            .is_some_and(|t| t.state == TaskState::Running && !t.activity_seen)
        {
            self.expect_uptake(id);
        }
        // A row left alone keeps what it had: nothing seen while the prompt
        // was pending counted as activity (see `apply`).
        if let Some(t) = written {
            self.emit(&format!("task.{}", t.state), Some(t.id));
        }
        self.refresh_live();
        Ok(true)
    }

    /// Send `task`'s pending `prompt` through its Claude session's inbox
    /// socket. The `agent.list` read just before gates it as herdr gates a
    /// typed prompt: a launching or blocked agent waits for the next status
    /// or reconcile. A resend says which attempt it is, since Claude drops
    /// the same text from the same sender within 30 seconds.
    async fn prompt_by_socket(
        &self,
        inbox: &crate::dispatch::Inbox,
        task: &Task,
        name: &str,
        prompt: &str,
    ) -> anyhow::Result<BySocket> {
        let timeout = self.settings.request_timeout;
        let agents = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        let Some(agent) = agents.into_iter().find(|a| a.name.as_deref() == Some(name)) else {
            // Gone: typing it fails the task as before.
            return Ok(BySocket::Type);
        };
        if agent.agent_status == AgentStatus::Blocked || agent.launch_pending {
            return Ok(BySocket::Wait);
        }
        let text = match self.unseen_prompt.get(&task.id) {
            Some(&resent) if resent > 0 => {
                format!(
                    "{prompt}\n\n(Sent again, attempt {}.)",
                    u16::from(resent) + 1
                )
            }
            _ => prompt.to_string(),
        };
        let shown = task.display_id();
        let send = inbox.send(self.connector.as_ref(), &shown, &text);
        // A send that does not finish in time is typed, like any other.
        Ok(match tokio::time::timeout(timeout, send).await {
            Ok(true) => BySocket::Sent(Box::new(agent)),
            _ => BySocket::Type,
        })
    }

    /// Send the pending prompts held after a trust answer once `settle` has
    /// passed. No status event may come in between: the agent sits idle at
    /// an empty input until it gets its prompt.
    async fn deliver_held_prompts(&mut self) -> anyhow::Result<()> {
        let due: Vec<i64> = self
            .dialog_answered
            .iter()
            .filter(|(_, at)| at.elapsed() >= self.settings.settle)
            .map(|(id, _)| *id)
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        let timeout = self.settings.request_timeout;
        let agents = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        let mut blocked_again = false;
        for id in due {
            let found = self
                .store
                .get_task(id)
                .ok()
                .flatten()
                .filter(|t| t.prompt_pending && t.state.occupies_pane())
                .and_then(|t| {
                    let agent = agents
                        .iter()
                        .find(|a| Some(&a.pane_id) == t.pane_id.as_ref())?;
                    Some((t, agent.agent_status))
                });
            blocked_again |= found
                .as_ref()
                .is_some_and(|(_, status)| *status == AgentStatus::Blocked);
            if let Some((task, status)) = found {
                // Its entry is still there and due, so the prompt goes in now
                // rather than being held again as a fresh answer.
                self.deliver_pending_prompt(task, status).await?;
                // Held at the next dialog, which herdr read idle (agy's
                // banner after its trust): read like one herdr reads blocked.
                blocked_again |= self
                    .store
                    .get_task(id)?
                    .is_some_and(|t| t.prompt_pending && t.state == TaskState::Blocked);
            }
            // Sent, or blocked again: leaving the block next starts a new hold.
            self.dialog_answered.remove(&id);
        }
        if blocked_again {
            // The next dialog of a chain: herdr reports no change when one
            // form replaces another, so without this read the task would
            // wait for the next reconcile. Once, after every due entry is
            // gone: the pass covers every task, and a hold it starts for a
            // task later in `due` must not be dropped by that task's turn.
            self.auto_answer().await?;
        }
        Ok(())
    }

    /// After the settle window, confirm with agent.list that the agent is still idle, at
    /// the `state_change_seq` it had when it was seen going idle, and that the sequence
    /// is past the task's baseline (`next_state`). Only then is the task done. A newer
    /// sequence while still idle means it worked again in between, unseen by events
    /// (a stream that fell behind, or polling): the window starts over. So does an
    /// unknown one (an idle event carries none): the window restarts from the
    /// sequence listed now, so a done task always sat a full window at one sequence.
    async fn confirm_pending_done(&mut self) -> anyhow::Result<()> {
        let due: Vec<i64> = self
            .pending_done
            .iter()
            .filter(|(_, (_, at))| at.elapsed() >= self.settings.settle)
            .map(|(id, _)| *id)
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        let timeout = self.settings.request_timeout;
        let agents = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        for id in due {
            let Some((seen_seq, _)) = self.pending_done.remove(&id) else {
                continue;
            };
            // Its idle is the save turn's end, not the task's.
            if self.saving.lock().unwrap().contains_key(&id) {
                continue;
            }
            let Ok(Some(task)) = self.store.get_task(id) else {
                continue;
            };
            let Some(agent) = agents
                .iter()
                .find(|a| Some(&a.pane_id) == task.pane_id.as_ref())
            else {
                continue;
            };
            // A stale task's pane handed to another agent: that agent's idle
            // is not this task's completion; `auto_close_stopped` fails it.
            if task.state == TaskState::Stale && holds_other_agent(&task, &agents, &agent.pane_id) {
                continue;
            }
            self.note_status(id, agent.agent_status, task.prompt_pending);
            // As in `reconcile`: a hard limit below closes the pane before
            // the next reconcile would otherwise capture a reported
            // conversation id, so take it from this same `agent.list` reply
            // first.
            let task = self.capture_agent_session(task, agent);
            let idle_like = matches!(agent.agent_status, AgentStatus::Idle | AgentStatus::Done);
            // No sequence yet (the candidate came from an event) is as good as a
            // moved one: the agent may have worked again inside the window.
            if idle_like && seen_seq != Some(agent.state_change_seq) {
                self.pending_done
                    .insert(id, (Some(agent.state_change_seq), Instant::now()));
                continue;
            }
            // Idle a whole window at the sequence its prompt went in at, never
            // seen at work: the agent did not take the prompt.
            if idle_like
                && task.state == TaskState::Running
                && !task.prompt_pending
                && !task.activity_seen
                && task.last_completion_seq == Some(agent.state_change_seq)
                && let Some(&resent) = self.unseen_prompt.get(&id)
            {
                // Codex typed it as a paste and did not submit it: press
                // Enter, as a person would, instead of pasting it again.
                if resent < MAX_PROMPT_RESENDS && self.submit_unsent_prompt(&task).await? {
                    self.unseen_prompt.insert(id, resent.saturating_add(1));
                    self.pending_done.insert(id, (seen_seq, Instant::now()));
                    continue;
                }
                self.prompt_not_taken(task, agent, resent, false).await?;
                continue;
            }
            // Seen blocked since its prompt, never at work: the blocked
            // spell may have been a startup dialog the prompt went into, its
            // Enter the answer. The prompt on screen shows it was taken.
            if idle_like
                && matches!(task.state, TaskState::Running | TaskState::Blocked)
                && !task.prompt_pending
                && task.activity_seen
                && let Some(&resent) = self.unseen_prompt.get(&id)
            {
                match self.prompt_on_screen(&task).await {
                    Ok(true) => {
                        self.unseen_prompt.remove(&id);
                    }
                    Ok(false) => {
                        self.prompt_not_taken(task, agent, resent, true).await?;
                        continue;
                    }
                    Err(err) => {
                        // Out of reach: look again next window.
                        self.pending_done.insert(id, (seen_seq, Instant::now()));
                        return Err(err);
                    }
                }
            }
            let observed = observed_from(agent);
            // Only a turn whose end was read says anything about the short
            // limits in a row: the others are not an end at all.
            let read = next_state(&task, &observed) == Some(TaskState::Done);
            let end = if read {
                match self.pane_end(&task).await {
                    Ok(end) => end,
                    Err(err) => {
                        // Not done yet: the pane could not be read because the
                        // machine is out of reach. Keep the task pending for the
                        // next settle check and let the caller reconnect.
                        self.pending_done.insert(id, (seen_seq, Instant::now()));
                        return Err(err);
                    }
                }
            } else {
                PaneEnd::Finished
            };
            let short = matches!(&end, PaneEnd::Limit(l, _) if !l.hard);
            if read && !short && !matches!(end, PaneEnd::ShellRunning | PaneEnd::Dialog(..)) {
                // A turn that ends on anything else than a short limit (one
                // waiting on a shell has not ended): the count starts over.
                self.rate_retries.remove(&id);
            }
            match end {
                PaneEnd::Limit(limit, _) if !limit.hard && self.retries_left(id) => {
                    self.retry_later(task, &observed, &limit)?;
                }
                PaneEnd::Limit(limit, text) => {
                    self.rate_retries.remove(&id);
                    if let Err(err) = self.wait_on_limit(task, &limit, false, &text).await {
                        // The pane could not be closed: look again next window.
                        self.pending_done.insert(id, (seen_seq, Instant::now()));
                        return Err(err);
                    }
                }
                PaneEnd::Said(said) => {
                    // As if the agent had run `pastor task done` with the
                    // message: the same row, round and event.
                    self.pending_done.remove(&id);
                    if let Err(err) = self.end_task(id, Some(said), EndBy::Agent) {
                        tracing::warn!(machine = %self.name, task = %task.display_id(), "end a task whose agent said DONE: {err:#}");
                    }
                }
                PaneEnd::Question(question) => {
                    let error = format!("agent asked: {question}");
                    let detail = serde_json::json!({ "question": question });
                    self.block_idle(task, &observed, error, detail)
                }
                PaneEnd::NotEnded(tail) => {
                    let error = format!(
                        "{IDLE_WITHOUT_TASK_DONE}: {}",
                        tail.lines().collect::<Vec<_>>().join(" / ")
                    );
                    let detail =
                        serde_json::json!({ "why": "idle_without_task_done", "tail": tail });
                    self.block_idle(task, &observed, error, detail)
                }
                PaneEnd::ShellRunning => {
                    // The agent ended its turn waiting on a background shell
                    // and takes it up again when the shell ends: not done.
                    // The window starts over at this sequence, so the next
                    // settle check looks again.
                    tracing::debug!(machine = %self.name, task = %task.display_id(), "background shell still running");
                    self.pending_done.insert(id, (seen_seq, Instant::now()));
                }
                PaneEnd::Dialog(dialog, keys) => {
                    // Answer it and look again after the window: the agent
                    // takes its turn up again, or ends it for good.
                    self.pending_done.insert(id, (seen_seq, Instant::now()));
                    let pane = task.pane_id.clone().unwrap_or_default();
                    self.press_dialog(&task, &pane, &dialog, keys).await?;
                }
                PaneEnd::DialogUp(name) => {
                    let error = format!("stopped under the {name} dialog");
                    let detail = serde_json::json!({ "why": "dialog_up", "dialog": name });
                    self.block_idle(task, &observed, error, detail)
                }
                // A task that must end with a summary and stops without one
                // fails, unless its worktree holds work not committed yet:
                // the idle may have been read too early, the agent cut off
                // before it committed. Then it is blocked, its pane and
                // session kept, so the agent can still finish with `task
                // done` and a person can still reach it.
                PaneEnd::Finished
                    if read
                        && !task.ended
                        && task.spec.summary == crate::task::SummaryMode::Require =>
                {
                    match self.holds_unsaved_work(&task).await {
                        Ok(true) => {
                            let error = format!(
                                "{}, with uncommitted files in its worktree",
                                crate::task::STOPPED_WITHOUT_SUMMARY
                            );
                            let detail =
                                serde_json::json!({ "why": "uncommitted_without_summary" });
                            self.block_idle(task, &observed, error, detail)
                        }
                        Ok(false) => self.apply(task, &observed),
                        Err(err) => {
                            // Out of reach: look again next window.
                            self.pending_done.insert(id, (seen_seq, Instant::now()));
                            return Err(err);
                        }
                    }
                }
                PaneEnd::Finished => self.apply(task, &observed),
            }
        }
        Ok(())
    }

    /// Whether task `id`, whose agent stopped on a short limit, is retried
    /// in its pane again: fewer than `[limits] rate_retries` so far.
    fn retries_left(&self, id: i64) -> bool {
        let Some(limits) = &self.settings.limits else {
            return false;
        };
        let done = self.rate_retries.get(&id).map_or(0, |r| r.attempts);
        done < limits.rate_retries
    }

    /// `task`'s agent stopped on `limit`, a 429 or 529 past its own
    /// retries: it keeps its pane, its slot and its state, and
    /// `RATE_RETRY_PROMPT` goes into its pane after the next wait of
    /// `[limits] rate_backoff`, or the wait the message names when that is
    /// longer (`send_rate_retries`). The baseline moves to the idle it was
    /// found at, as for a question (`block_on_question`), so that idle is
    /// not read as done meanwhile. Emits `task.rate_limited`.
    fn retry_later(
        &mut self,
        mut task: Task,
        observed: &Observed,
        limit: &crate::limit::Limit,
    ) -> anyhow::Result<()> {
        let limits = self.settings.limits.clone().unwrap_or_default();
        let attempt = self.rate_retries.get(&task.id).map_or(0, |r| r.attempts) + 1;
        let now = Utc::now();
        let mut wait = limits.rate_backoff_for(attempt);
        if let Some(named) = limit.until.and_then(|u| (u - now).to_std().ok()) {
            wait = wait.max(named);
        }
        let retry_at = now
            .checked_add_signed(chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::MAX))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        if let Observed::Status {
            state_change_seq,
            completion_seq,
            ..
        } = observed
        {
            task.last_completion_seq = completion_seq.or(*state_change_seq);
        }
        task.activity_seen = false;
        self.store.update_task(&mut task)?;
        self.rate_retries.insert(
            task.id,
            RateRetry {
                attempts: attempt,
                due: Some(Instant::now() + wait),
            },
        );
        tracing::warn!(machine = %self.name, task = %task.display_id(), attempt, %retry_at, line = %limit.line, "short usage limit; retrying in the pane");
        self.emit_with(
            "task.rate_limited",
            Some(task.id),
            Some(serde_json::json!({
                "line": limit.line,
                "attempt": attempt,
                "retry_at": retry_at,
            })),
        );
        Ok(())
    }

    /// Send `RATE_RETRY_PROMPT`, then Enter, into the pane of each task
    /// whose short-limit wait has passed (`retry_later`), as `task send`
    /// would, with `task.input`. A task that is no longer running with a
    /// pane is not sent anything. A send that fails is tried again on the
    /// next tick.
    async fn send_rate_retries(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let due: Vec<i64> = self
            .rate_retries
            .iter()
            .filter(|(_, r)| r.due.is_some_and(|d| d <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let task = match self.store.get_task(id) {
                Ok(Some(t)) if t.state == TaskState::Running => t,
                Ok(_) => {
                    self.rate_retries.remove(&id);
                    continue;
                }
                Err(err) => return Err(err),
            };
            let Some(pane) = task.pane_id.clone() else {
                self.rate_retries.remove(&id);
                continue;
            };
            let text = crate::task::RATE_RETRY_PROMPT;
            // By socket, each attempt says which it is: Claude drops the same
            // text from the same sender within 30 seconds, and the waits
            // between attempts can be shorter.
            let attempt = self.rate_retries.get(&id).map_or(1, |r| r.attempts);
            let socket_text = if attempt > 1 {
                format!("{text} (Attempt {attempt}.)")
            } else {
                text.to_string()
            };
            let by_socket = self.text_by_socket(&task, &socket_text).await;
            let keys = if by_socket {
                vec![]
            } else {
                vec!["Enter".to_string()]
            };
            if !by_socket {
                let timeout = self.settings.request_timeout;
                tokio::time::timeout(timeout, async {
                    self.connector.pane_send_text(&pane, text).await?;
                    self.connector.pane_send_keys(&pane, &keys).await
                })
                .await
                .map_err(|_| TimedOut("pane input", timeout))??;
            }
            if let Some(r) = self.rate_retries.get_mut(&id) {
                r.due = None;
            }
            tracing::info!(machine = %self.name, task = %task.display_id(), "sent the retry prompt after a short usage limit");
            self.emit_with(
                "task.input",
                Some(id),
                Some(serde_json::json!({
                    "text_len": if by_socket { socket_text.chars().count() } else { text.chars().count() },
                    "keys": keys,
                    "rate_retry": true,
                    "socket": by_socket,
                })),
            );
        }
        Ok(())
    }

    /// Codex idle at the sequence its prompt went in, never at work: if the
    /// prompt sits in its input box (`task::prompt_unsent_in_input`), press
    /// Enter to submit it, and say so with `task.input`. Returns whether it
    /// did. A pane that cannot be read, or another kind of agent, leaves the
    /// prompt to be sent again as before.
    async fn submit_unsent_prompt(&mut self, task: &Task) -> anyhow::Result<bool> {
        if self.settings.agents.kind(&task.spec.agent) != "codex" {
            return Ok(false);
        }
        let (Some(pane), Some(name)) = (
            task.pane_id.clone(),
            task.agent_name
                .clone()
                .or_else(|| Some(Task::agent_name_for(task.id))),
        ) else {
            return Ok(false);
        };
        let timeout = self.settings.request_timeout;
        let screen = match tokio::time::timeout(timeout, self.connector.agent_read(&name, 100))
            .await
            .map_err(|_| TimedOut("agent.read", timeout))?
        {
            Ok(screen) => screen,
            Err(err) if err.is_transport() => return Err(err.into()),
            Err(err) => {
                tracing::warn!(machine = %self.name, task = %task.display_id(), %err, "read pane for an unsubmitted prompt");
                return Ok(false);
            }
        };
        // Codex's folder-trust dialog reads idle, and Enter there would trust
        // the folder: a dialog pastor holds the prompt for is not an input.
        let answered = self.answered_dialogs(task)?;
        let hold = self
            .settings
            .agents
            .startup_markers(&task.spec.agent, &answered);
        if crate::config::shows_startup_question(&screen, &hold) {
            return Ok(false);
        }
        if !crate::task::prompt_unsent_in_input(&screen, &crate::task::pending_prompt(task)) {
            return Ok(false);
        }
        let keys = vec!["Enter".to_string()];
        tokio::time::timeout(timeout, self.connector.pane_send_keys(&pane, &keys))
            .await
            .map_err(|_| TimedOut("pane.send_keys", timeout))??;
        tracing::info!(machine = %self.name, task = %task.display_id(), "the prompt sat in the agent's input; pressed Enter to submit it");
        self.emit_with(
            "task.input",
            Some(task.id),
            Some(serde_json::json!({"keys": keys, "submit": true})),
        );
        Ok(true)
    }

    /// The agent of `task` sits at an empty input with the prompt it was
    /// given nowhere (see `unseen_prompt`): send it again, or, after
    /// `MAX_PROMPT_RESENDS`, mark the task `blocked` so it does not read as
    /// `running` with nothing pending. The baseline stays at the idle it sits
    /// at, so `next_state` holds it there until the agent moves. With
    /// `blocked_seen`, the agent was seen `blocked` since the prompt but the
    /// prompt is not on its screen (`prompt_on_screen`): that spell was no
    /// work on it, so the task is `running` again with no activity; and once
    /// the resends are spent, `blocked` with no activity and its baseline at
    /// the idle `agent` sits at, so `next_state` holds it there.
    async fn prompt_not_taken(
        &mut self,
        task: Task,
        agent: &AgentInfo,
        resent: u8,
        blocked_seen: bool,
    ) -> anyhow::Result<()> {
        let id = task.id;
        let status = agent.agent_status;
        let idle_seq = agent.completion_seq.or(Some(agent.state_change_seq));
        let unchanged = |t: &Task| {
            !t.prompt_pending
                && if blocked_seen {
                    matches!(t.state, TaskState::Running | TaskState::Blocked)
                } else {
                    t.state == TaskState::Running && !t.activity_seen
                }
        };
        if resent >= MAX_PROMPT_RESENDS {
            self.unseen_prompt.remove(&id);
            let message = format!(
                "agent did not take its prompt ({} sent); send it with `pastor task send`",
                u16::from(resent) + 1
            );
            tracing::warn!(machine = %self.name, task = %task.display_id(), "{message}");
            let written = write_task(&self.store, task, |t| {
                if !unchanged(t) {
                    return false;
                }
                t.state = TaskState::Blocked;
                t.error = Some(message.clone());
                // A blocked spell before this counted as activity: neither it
                // nor the sequence it moved may read as the task done.
                t.activity_seen = false;
                t.last_completion_seq = idle_seq;
                true
            })?;
            if let Some(t) = written {
                self.emit("task.blocked", Some(t.id));
                self.refresh_live();
            }
            return Ok(());
        }
        tracing::warn!(machine = %self.name, task = %task.display_id(), "agent did not take its prompt; sending it again");
        self.unseen_prompt.insert(id, resent.saturating_add(1));
        let pending = write_task(&self.store, task, |t| {
            let resend = unchanged(t);
            if resend {
                t.prompt_pending = true;
                t.state = TaskState::Running;
                t.activity_seen = false;
            }
            resend
        })?;
        if let Some(task) = pending {
            self.deliver_pending_prompt(task, status).await?;
        }
        Ok(())
    }

    /// The agent of `task` went `blocked` after its prompt went in, before
    /// it was seen at work. If its screen shows one of its startup dialogs
    /// (`Agents::startup_markers`), herdr read it ready before it read the
    /// dialog, and the prompt went into the dialog, not to the agent. Then
    /// the prompt is pending again and the task `blocked`, with no activity:
    /// the dialog is answered as any startup block is (`auto_answer`, or a
    /// person), and the prompt goes in after it. Returns whether it did so;
    /// `false` leaves the status to the usual state machine.
    async fn prompt_in_dialog(&mut self, task: &Task) -> anyhow::Result<bool> {
        if task.prompt_pending || task.state != TaskState::Running || task.activity_seen {
            return Ok(false);
        }
        let Some(&resent) = self.unseen_prompt.get(&task.id) else {
            return Ok(false);
        };
        let answered = self.answered_dialogs(task)?;
        let hold = self
            .settings
            .agents
            .startup_markers(&task.spec.agent, &answered);
        if hold.is_empty() {
            return Ok(false);
        }
        let name = task
            .agent_name
            .clone()
            .unwrap_or_else(|| Task::agent_name_for(task.id));
        let timeout = self.settings.request_timeout;
        let screen = match tokio::time::timeout(timeout, self.connector.agent_read(&name, 100))
            .await
            .map_err(|_| TimedOut("agent.read", timeout))?
        {
            Ok(screen) => screen,
            Err(err) if err.is_transport() => return Err(err.into()),
            Err(err) => {
                tracing::warn!(machine = %self.name, task = %task.display_id(), %err, "read pane for a startup question");
                return Ok(false);
            }
        };
        if !crate::config::shows_startup_question(&screen, &hold) {
            return Ok(false);
        }
        let written = write_task(&self.store, task.clone(), |t| {
            if t.prompt_pending || t.state != TaskState::Running || t.activity_seen {
                return false;
            }
            t.prompt_pending = true;
            t.state = TaskState::Blocked;
            true
        })?;
        let Some(t) = written else {
            return Ok(false);
        };
        tracing::warn!(machine = %self.name, task = %t.display_id(), "the prompt went into a startup dialog; it waits until the dialog is answered");
        // The resend says which attempt it is (`prompt_by_socket`).
        self.unseen_prompt.insert(t.id, resent.saturating_add(1));
        self.pending_done.remove(&t.id);
        self.emit("task.blocked", Some(t.id));
        self.refresh_live();
        Ok(true)
    }

    /// Does the screen of `task`'s agent show the prompt it was given? An
    /// agent that took it shows it as its first message. Only the start of
    /// the prompt is looked for, whitespace aside, since the pane wraps it.
    /// A pane that cannot be read for any other reason than the transport
    /// counts as showing it: the task ends as it did before this check.
    async fn prompt_on_screen(&self, task: &Task) -> anyhow::Result<bool> {
        let name = task
            .agent_name
            .clone()
            .unwrap_or_else(|| Task::agent_name_for(task.id));
        let timeout = self.settings.request_timeout;
        let screen = match tokio::time::timeout(timeout, self.connector.agent_read(&name, 100))
            .await
            .map_err(|_| TimedOut("agent.read", timeout))?
        {
            Ok(screen) => screen,
            Err(err) if err.is_transport() => return Err(err.into()),
            Err(err) => {
                tracing::warn!(machine = %self.name, task = %task.display_id(), %err, "read pane for the prompt");
                return Ok(true);
            }
        };
        Ok(crate::task::shows_prompt(
            &screen,
            &crate::task::prompt_to_send(task),
        ))
    }

    /// The usage limit `task`'s agent stopped on, if `text`, the end of its
    /// pane, ends on one; a login screen or a lost login reads as a limit
    /// that is signed out (`limit::signed_out_in`). None on a task its agent
    /// ended (`task done`), and none where no table is kept
    /// (`MachineSettings::limits`).
    fn limit_in(&self, task: &Task, text: &str) -> Option<crate::limit::Limit> {
        if task.ended || self.settings.limits.is_none() {
            return None;
        }
        let agents = &self.settings.agents;
        let kind = agents.kind(&task.spec.agent);
        crate::limit::signed_out_in(kind, text, &agents.login_markers(&task.spec.agent))
            .or_else(|| crate::limit::limit_in(kind, text, Utc::now()))
    }

    /// Keep `limit`, which `task`'s agent stopped on, in the table under the
    /// account its agent names here (`Agents::limit_key`), with
    /// `agent.exhausted` (`agent.signed_out` for a signed-out agent) and
    /// `task.limited`, and answer the row. A limit
    /// that is not hard (a 429 or 529 past the agent's own retries) is kept as
    /// one with no reset named. The row is answered even when the store
    /// refuses it: the task waits all the same.
    fn keep_limit(&self, task: &Task, limit: &crate::limit::Limit) -> crate::limit::AccountLimit {
        let now = Utc::now();
        let mut limit = limit.clone();
        if !limit.hard {
            limit.until = None;
        }
        let agents = &self.settings.agents;
        let key = if limit.signed_out {
            agents.signed_out_key(&self.name, &task.spec.agent)
        } else {
            agents.limit_key(&self.name, &task.spec.agent)
        };
        let config = self.settings.limits.clone().unwrap_or_default();
        let row = crate::limit::AccountLimit::of(
            &key,
            task.model(),
            &limit,
            Some(task.id),
            Some(&self.name),
            Some(&task.spec.agent),
            now,
            &config,
        );
        match self.store.record_limit(&row) {
            Ok(()) => {
                if row.signed_out {
                    tracing::warn!(machine = %self.name, task = %task.display_id(), account = %row.account, agent = %task.spec.agent, "agent signed out");
                } else {
                    tracing::warn!(machine = %self.name, task = %task.display_id(), account = %row.account, model = ?row.model, retry_at = %row.retry_at, "account exhausted");
                }
                self.emit_with(
                    row.kept_event(),
                    Some(task.id),
                    Some(row.event_detail(None)),
                );
            }
            Err(err) => {
                tracing::error!(machine = %self.name, task = %task.display_id(), %err, "keep the usage limit")
            }
        }
        self.emit_with("task.limited", Some(task.id), Some(row.event_detail(None)));
        row
    }

    /// `task`, whose agent stopped on `limit`, goes `waiting`
    /// (`TaskState::Waiting`): the limit is kept (`keep_limit`), the pane
    /// closed as a pause closes it (`interrupt_and_close`), the worktree
    /// kept, and the row pinned to this machine until the limit's
    /// `retry_at`, with its slot freed. `fresh_session` drops a session its
    /// agent never got a prompt in: it died at the start of a new one, and
    /// the task starts again from its prompt. A task with a fallback list
    /// whose reset is not near is left for the next dispatch pass to move
    /// to its next model (`stop_on_limit`), with `text`, its pane, kept for
    /// the agent that takes it over.
    async fn wait_on_limit(
        &mut self,
        task: Task,
        limit: &crate::limit::Limit,
        fresh_session: bool,
        text: &str,
    ) -> anyhow::Result<()> {
        let row = self.keep_limit(&task, limit);
        self.interrupt_and_close(&task).await?;
        self.pending_done.remove(&task.id);
        self.unseen_prompt.remove(&task.id);
        let now = Utc::now();
        let waiting = write_task(&self.store, task, |t| {
            if !t.state.occupies_pane() || t.ended {
                return false;
            }
            into_waiting(t, &row, fresh_session, now);
            self.stop_on_limit(t, &row, limit, text, now);
            true
        })?;
        if let Some(t) = waiting {
            self.emit_waiting(&t, &row, now);
            self.refresh_live();
        }
        Ok(())
    }

    /// Keep with `t`, which just went `waiting` on `row` (read from
    /// `limit`), what its next round needs (`task::LimitStop`): wait for
    /// its own model when the reset is within `[limits] wait_under` or it
    /// has no fallback list, else move to its next model on the next
    /// dispatch pass, which is due at once (`waiting_until` is `now`).
    fn stop_on_limit(
        &self,
        t: &mut Task,
        row: &crate::limit::AccountLimit,
        limit: &crate::limit::Limit,
        text: &str,
        now: DateTime<Utc>,
    ) {
        let config = self.settings.limits.clone().unwrap_or_default();
        let (then, waited) = next_after_limit(t, row, &config, now);
        if then == crate::task::NextRound::Switch {
            t.waiting_until = Some(now);
        }
        let lines = config.handover_lines as usize;
        t.spec.rounds.stop = Some(Box::new(crate::task::LimitStop {
            agent: t.spec.agent.clone(),
            model: t.model().map(str::to_string),
            why: if limit.signed_out {
                "signed_out"
            } else if limit.hard {
                "limit"
            } else {
                "rate_limit"
            }
            .into(),
            line: row.line.clone(),
            until: row.retry_at,
            tail: crate::task::pane_tail(text, lines).join("\n"),
            then,
            waited,
            handover: None,
        }));
    }

    /// `task.waiting` about `t`, which waits on `row`, with what a board
    /// needs to show it: `waiting 03:00`. None for a task that moves to its
    /// next model instead: the dispatch pass says what it does.
    fn emit_waiting(&self, t: &Task, row: &crate::limit::AccountLimit, now: DateTime<Utc>) {
        let stop = t.spec.rounds.stop.as_deref();
        if stop.is_some_and(|s| s.then == crate::task::NextRound::Switch) {
            return;
        }
        let waited = stop.and_then(|s| s.waited.clone());
        let why = if waited.is_some() {
            "reset_soon"
        } else {
            "no_fallback"
        };
        let shown = if row.signed_out {
            format!("waiting: {}", row.short_note(now))
        } else {
            format!("waiting {}", crate::limit::local_time(row.retry_at, now))
        };
        tracing::info!(machine = %self.name, task = %t.display_id(), until = %row.retry_at, why, "waiting for a usage limit to reset");
        self.emit_with(
            "task.waiting",
            Some(t.id),
            Some(serde_json::json!({
                "why": why,
                "waited": waited,
                "account": row.account,
                "model": row.model,
                "until": row.retry_at,
                "signed_out": row.signed_out,
                "shown": shown,
            })),
        );
    }

    /// Interrupt `task`'s agent (`esc`, so Claude ends its turn and saves
    /// the conversation as it stands), then close its pane, which ends the
    /// agent. A worktree stays on disk: only `worktree.remove` deletes one.
    /// A pane already gone is what this wanted.
    async fn interrupt_and_close(&self, task: &Task) -> anyhow::Result<()> {
        let Some(pane) = task.pane_id.as_deref() else {
            return Ok(());
        };
        let name = task.display_id();
        let timeout = self.settings.request_timeout;
        let keys = ["esc".to_string()];
        match tokio::time::timeout(timeout, self.connector.pane_send_keys(pane, &keys))
            .await
            .map_err(|_| TimedOut("pane.send_keys", timeout))?
        {
            Ok(()) => {}
            // Gone already: nothing left to interrupt.
            Err(err) if err.code() == Some("pane_not_found") => {}
            Err(err) => {
                return Err(
                    anyhow::Error::from(err).context(format!("interrupt the agent of {name}"))
                );
            }
        }
        let left = self.find_leftovers(Some(task), pane, &name, None).await;
        self.close_pane_of(pane, &name).await?;
        self.end_leftovers(left).await;
        Ok(())
    }

    /// How the task's agent ended its turn, read from the tail of its pane:
    /// on a question (`task::trailing_question`), waiting on a background
    /// shell (`task::background_shell_running`), or finished. An agent that
    /// is not Claude is read for agy's permission prompts
    /// (`task::permission_question`) and its `· N task` footer
    /// (`task::tasks_running`) too, and is finished only once it said so
    /// (`not_ended`). A pane herdr
    /// cannot read is finished, so the task is done as it would have been
    /// without this check. A lost connection or a read that never answers is
    /// an outage (`is_outage`): the task is not settled on it, and the caller
    /// reconnects.
    async fn pane_end(&self, task: &Task) -> anyhow::Result<PaneEnd> {
        let Some(target) = task.agent_name.as_deref() else {
            return Ok(PaneEnd::Finished);
        };
        let timeout = self.settings.request_timeout;
        match tokio::time::timeout(timeout, self.connector.agent_read(target, 100)).await {
            Ok(Ok(text)) => {
                let claude = self.settings.agents.kind(&task.spec.agent) == "claude";
                // Under a dialog Claude drew in place of its input box, the
                // agent does not go on until it is answered, whatever else
                // the pane says.
                if claude && let Some((dialog, keys)) = self.pick_dialog(task, &text, true)? {
                    return Ok(PaneEnd::Dialog(dialog, keys));
                }
                if (claude && crate::task::background_shell_running(&text))
                    || (!claude && crate::task::tasks_running(&text))
                {
                    return Ok(PaneEnd::ShellRunning);
                }
                if claude && let Some(name) = self.dialog_up(task, &text) {
                    return Ok(PaneEnd::DialogUp(name));
                }
                // A task its agent ended (`task done`) is done whatever its
                // pane says.
                if let Some(limit) = self.limit_in(task, &text) {
                    return Ok(PaneEnd::Limit(limit, text));
                }
                // Its last line is DONE: it said it is finished, in Claude's
                // words or agy's, and `pastor task done` is left to it.
                if matches!(
                    self.settings.agents.kind(&task.spec.agent),
                    "claude" | "agy"
                ) && let Some(said) = crate::task::trailing_done(&text, claude)
                    // A task that must say what it did is not finished by the
                    // word alone.
                    && (said != crate::task::DONE_WORD
                        || task.spec.summary != crate::task::SummaryMode::Require)
                {
                    return Ok(PaneEnd::Said(said));
                }
                // Kept only when the task really is done: a tail left by a
                // question would reach a later failed task's finish command.
                let question = crate::task::trailing_question(&text)
                    .or_else(|| (!claude).then(|| crate::task::permission_question(&text))?);
                if question.is_none() && !claude && not_ended(task) {
                    let tail = crate::task::pane_tail(&text, IDLE_TAIL_LINES).join("\n");
                    return Ok(PaneEnd::NotEnded(tail));
                }
                Ok(match question {
                    Some(question) => PaneEnd::Question(question),
                    None => {
                        self.store.note_pane_tail(task.id, &text);
                        PaneEnd::Finished
                    }
                })
            }
            Ok(Err(err)) if err.is_transport() => Err(err.into()),
            Ok(Err(err)) => {
                tracing::warn!(machine = %self.name, task = %task.display_id(), %err, "read pane for a question");
                Ok(PaneEnd::Finished)
            }
            Err(_) => Err(TimedOut("agent.read", timeout).into()),
        }
    }

    /// Mark a task whose agent went idle on a question, or not ended
    /// (`PaneEnd::NotEnded`), `blocked` instead of `done`, so `task send`
    /// can answer it, with `error` and `task.blocked` carrying `detail`. The
    /// baseline moves to the idle it was found at, as a completion would
    /// move it, so `next_state` holds the task here until the agent moves
    /// again, and the answer's own work is what finishes it next.
    fn block_idle(
        &mut self,
        mut task: Task,
        observed: &Observed,
        error: String,
        detail: serde_json::Value,
    ) {
        if let Observed::Status {
            state_change_seq,
            completion_seq,
            ..
        } = observed
        {
            task.last_completion_seq = completion_seq.or(*state_change_seq);
        }
        task.activity_seen = false;
        task.state = TaskState::Blocked;
        task.finished_at = None;
        task.error = Some(error);
        if let Err(err) = self.store.update_task(&mut task) {
            tracing::error!(%err, "update task");
            return;
        }
        self.emit_with("task.blocked", Some(task.id), Some(detail));
        self.refresh_live();
    }

    /// Remember whether the agent of task `id` is between turns; see
    /// `Actor::idle_agents`. `prompt_pending`: the task's prompt has not gone
    /// in yet, so work seen now is not work on it.
    fn note_status(&mut self, id: i64, status: AgentStatus, prompt_pending: bool) {
        match status {
            AgentStatus::Idle | AgentStatus::Done => {
                self.idle_agents.insert(id);
                self.picker_read.remove(&id);
            }
            AgentStatus::Working | AgentStatus::Blocked => {
                if status == AgentStatus::Working {
                    self.picker_read.remove(&id);
                }
                self.idle_agents.remove(&id);
                // At work, it took its prompt (one already given: a pending
                // one keeps its resend count for the attempt it goes in as).
                // Blocked, it may sit at a startup dialog the prompt went
                // into: that is checked (`prompt_in_dialog`,
                // `prompt_on_screen`), not assumed.
                if status == AgentStatus::Working && !prompt_pending {
                    self.unseen_prompt.remove(&id);
                }
            }
            AgentStatus::Unknown => {}
        }
    }

    /// Record `task`'s agent's own session, the first time an `agent.list`
    /// entry pastor reads for it (reconcile, the settle check, a close)
    /// reports one it can use (`task::reported_session`); anything else, or
    /// no report at all
    /// (a machine with no such hook installed, or one that has not run yet),
    /// leaves the task with none: a later pause is then refused
    /// (`task::why_not_pausable`) and `task attach` fails with `no_agent`,
    /// as before this; only a usage-limit wait still starts the task fresh
    /// from its prompt, as it always has.
    fn capture_agent_session(&self, task: Task, agent: &AgentInfo) -> Task {
        let kind = self.settings.agents.kind(&task.spec.agent);
        let Some(value) = crate::task::reported_session(kind, agent) else {
            return task;
        };
        if task.spec.session_id.as_deref() == Some(value) {
            return task;
        }
        let value = value.to_string();
        let name = task.display_id();
        match write_task(&self.store, task.clone(), |t| {
            if t.spec.session_id.as_deref() == Some(value.as_str()) {
                return false;
            }
            t.spec.session_id = Some(value.clone());
            true
        }) {
            Ok(Some(written)) => written,
            Ok(None) => task,
            Err(err) => {
                tracing::error!(%err, task = %name, "record agent session");
                task
            }
        }
    }

    /// The agent of task `id` was just given its prompt: watch that it takes
    /// it (see `unseen_prompt`). The settle window starts now, since an agent
    /// that lost the prompt sends no event to start one.
    fn expect_uptake(&mut self, id: i64) {
        self.unseen_prompt.entry(id).or_insert(0);
        self.pending_done
            .entry(id)
            .or_insert((None, Instant::now()));
    }

    fn apply(&mut self, mut task: Task, observed: &Observed) {
        // Any `working` or `blocked` after the prompt is activity, whether an
        // event or `agent.list` showed it; `unknown` never is. Before the
        // prompt reached the agent (`prompt_pending`) nothing counts.
        // It is stored with the task (see `Task::activity_seen`), so the first
        // sighting is written even when the state does not change.
        let first_activity = matches!(observed, Observed::Status { status, .. } if status.is_activity())
            && !task.prompt_pending
            && !task.activity_seen;
        if first_activity {
            task.activity_seen = true;
        }
        let Some(to) = next_state(&task, observed) else {
            if first_activity {
                let written = write_task(&self.store, task, |t| {
                    let wanted = !t.prompt_pending && t.state.is_open();
                    t.activity_seen |= wanted;
                    wanted
                });
                if let Err(err) = written {
                    tracing::error!(%err, "record agent activity");
                }
            }
            return;
        };
        // An agent that stops without a summary on a task that requires one
        // did not finish: the agent's own `task done --summary` is the only
        // way such a task ends `done` (`end_task`).
        let to = if to == TaskState::Done
            && !task.ended
            && task.spec.summary == crate::task::SummaryMode::Require
        {
            task.error = Some(crate::task::STOPPED_WITHOUT_SUMMARY.into());
            TaskState::Failed
        } else {
            to
        };
        if to == TaskState::Done || !to.is_open() {
            // The next completion needs activity of its own.
            task.activity_seen = false;
        }
        if !to.is_open() {
            self.idle_agents.remove(&task.id);
        }
        if !matches!(to, TaskState::Running | TaskState::Blocked) {
            self.unseen_prompt.remove(&task.id);
        }
        if let Observed::Status {
            state_change_seq,
            completion_seq,
            ..
        } = observed
            && to == TaskState::Done
        {
            // The next completion of this task (a done agent that is given
            // more to do, say) has to move past this one.
            task.last_completion_seq = completion_seq.or(*state_change_seq);
        }
        let from = task.state;
        task.state = to;
        if to == TaskState::Closed {
            // `done -> closed` ends the cycle that finished at `finished_at`;
            // the pane going away later is not when the work finished.
            task.finished_at = task.finished_at.or_else(|| Some(Utc::now()));
        } else if matches!(to, TaskState::Done | TaskState::Failed) {
            task.finished_at = Some(Utc::now());
        } else {
            // The task is open again (a `Done` agent that picked the work back
            // up, say): the old finish time belongs to a cycle that is over, and
            // leaving it there reads as "finished at ..." on a running task.
            task.finished_at = None;
        }
        if from == TaskState::Blocked && matches!(to, TaskState::Running | TaskState::Done) {
            // "agent blocked during startup; answer its prompt" was advice about
            // a state the task has left; it is not an error on a running task.
            task.error = None;
        }
        if from == TaskState::Blocked && to == TaskState::Running {
            // The timeout counts the agent's own time, not a person's: a
            // block cleared past the old deadline must not read as already
            // timed out the moment it starts working again.
            task.started_at = Some(Utc::now());
        }
        if to == TaskState::Failed && task.error.is_none() {
            task.error = Some("agent process exited".into());
        }
        if let Err(err) = self.store.update_task(&mut task) {
            tracing::error!(%err, "update task");
            return;
        }
        self.emit(&format!("task.{to}"), Some(task.id));
        self.refresh_live();
    }

    /// Compare open tasks with live agents. Missing agent means the task failed while we
    /// were away; a present agent's status is applied like an event, except idle, which
    /// goes through the settle window. Long-running tasks become stale.
    ///
    /// Returns whether it adopted a pane (a `Starting` task found by agent
    /// name): the event subscription open at that moment does not cover the
    /// pane, so the caller must resubscribe or the task is only ever seen by
    /// the next reconcile.
    async fn reconcile(&mut self) -> anyhow::Result<bool> {
        let mut adopted = false;
        let timeout = self.settings.request_timeout;
        let agents: Vec<AgentInfo> = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        // Tasks whose agent this list shows blocked, for the limit picker.
        let mut blocked = Vec::new();
        for task in self.store.tasks_on_machine(&self.name)? {
            let Some(pane_id) = task.pane_id.clone() else {
                // Only a `Starting` task can occupy a pane slot with no pane recorded
                // (see `TaskState::occupies_pane`): `run_dispatch` persists `Starting`
                // with `machine` set before its first herdr call, so a daemon crash
                // mid-dispatch leaves exactly this row behind. Adopt the agent it
                // must have started, found by the name dispatch gives it (`t-<id>`);
                // if none exists the dispatch never got that far and the task failed.
                let name = Task::agent_name_for(task.id);
                let mut t = task;
                match agents
                    .iter()
                    .find(|a| a.name.as_deref() == Some(name.as_str()))
                {
                    Some(agent) => {
                        t.pane_id = Some(agent.pane_id.clone());
                        t.workspace_id = Some(agent.workspace_id.clone());
                        // Set from the same name just matched on, not read back from
                        // the row: adoption must not depend on `agent_name` already
                        // being persisted (belt and suspenders alongside
                        // `run_dispatch` persisting it up front; see the comment
                        // there).
                        t.agent_name = Some(name.clone());
                        // Whether dispatch got as far as the prompt is unknown, so
                        // count from now: only work seen after adoption completes it.
                        // An agent that had already finished is left to go stale.
                        t.last_completion_seq = Some(agent.state_change_seq);
                        // Still launching, it cannot have taken the prompt: its launch
                        // ending moves the sequence past the baseline above, which
                        // must not read as this task done. The pending delivery sends
                        // the prompt once it is ready and resets the baseline.
                        if agent.launch_pending {
                            t.prompt_pending = true;
                        }
                        let idle_like = matches!(
                            agent.agent_status,
                            crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done
                        );
                        if idle_like {
                            // Adopting proves dispatch reached at least `agent.start`;
                            // land the task at `Running` immediately, the same target
                            // a successful dispatch would have recorded, instead of
                            // applying the agent's raw idle/done status straight
                            // through: that could mark it `Done` with no settle
                            // window, or leave it stuck as `Starting` forever
                            // otherwise (stale only covers Running/Blocked). An
                            // observation with no sequence forces exactly the
                            // `Running` transition; the agent's real sequence is
                            // picked up through the settle window below instead,
                            // same as any other task.
                            t.state = next_state(
                                &t,
                                &Observed::Status {
                                    status: agent.agent_status,
                                    state_change_seq: None,
                                    completion_seq: None,
                                },
                            )
                            .expect("adopting a Starting task always reaches Running");
                        }
                        if let Err(err) = self.store.update_task(&mut t) {
                            tracing::error!(%err, task = %t.display_id(), "adopt starting task");
                            continue;
                        }
                        adopted = true;
                        if idle_like {
                            self.emit("task.running", Some(t.id));
                            // Never mark Done from a reconcile directly: the settle
                            // window applies here too, same as the pane-known branch
                            // below.
                            self.pending_done
                                .entry(t.id)
                                .or_insert((Some(agent.state_change_seq), Instant::now()));
                        } else {
                            self.apply(t, &observed_from(agent));
                        }
                    }
                    None => {
                        t.error = Some(format!(
                            "dispatch of {} was interrupted before an agent started",
                            t.display_id()
                        ));
                        self.apply(t, &Observed::PaneExited { agent_idle: false });
                    }
                }
                continue;
            };
            match agents.iter().find(|a| a.pane_id == pane_id) {
                None => {
                    // Route through the state machine instead of forcing `Failed`
                    // directly: a `Done` task whose pane later vanishes has already
                    // finished its work and must close cleanly, not be reported as
                    // failed.
                    let mut t = task;
                    let exited = Observed::PaneExited {
                        agent_idle: self.idle_agents.contains(&t.id),
                    };
                    if next_state(&t, &exited) == Some(TaskState::Failed) {
                        t.error = Some(format!(
                            "agent {} not found on machine {}",
                            t.agent_name
                                .clone()
                                .unwrap_or_else(|| Task::agent_name_for(t.id)),
                            self.name
                        ));
                    }
                    self.apply(t, &exited);
                }
                // As in `confirm_pending_done`: another agent's status is
                // not a stale task's; `auto_close_stopped` repairs the row.
                Some(_)
                    if task.state == TaskState::Stale
                        && holds_other_agent(&task, &agents, &pane_id) =>
                {
                    self.pending_done.remove(&task.id);
                }
                Some(agent) => {
                    self.note_status(task.id, agent.agent_status, task.prompt_pending);
                    let task = self.capture_agent_session(task, agent);
                    if self.saving_saw(task.id, agent.agent_status) {
                        continue;
                    }
                    if agent.agent_status == AgentStatus::Blocked {
                        blocked.push(task.id);
                    }
                    // Before the timeout check: a task that sat blocked past its
                    // timeout has not started its work yet.
                    if task.prompt_pending
                        && self
                            .deliver_pending_prompt(task.clone(), agent.agent_status)
                            .await?
                    {
                        continue;
                    }
                    if agent.agent_status == AgentStatus::Blocked
                        && self.prompt_in_dialog(&task).await?
                    {
                        continue;
                    }
                    // Only a running task times out, counted from its latest
                    // start (dispatch, resume or reopen): a blocked one waits for
                    // a person, not for its agent. That includes one this list is
                    // the first to show blocked (a missed event): `apply` below
                    // marks it so.
                    let timed_out = task
                        .started_at
                        .map(|s| (Utc::now() - s).num_seconds() as u64 > task.spec.timeout_secs)
                        .unwrap_or(false);
                    if timed_out
                        && task.state == TaskState::Running
                        && agent.agent_status != crate::herdr::AgentStatus::Blocked
                    {
                        let written = write_task(&self.store, task, |t| {
                            let running = t.state == TaskState::Running;
                            if running {
                                t.state = TaskState::Stale;
                            }
                            running
                        });
                        match written {
                            Ok(Some(t)) => self.emit("task.stale", Some(t.id)),
                            Ok(None) => {}
                            // One row must not stop the rest of the pass, nor
                            // the live count after it.
                            Err(err) => tracing::error!(machine = %self.name, %err, "mark stale"),
                        }
                        continue;
                    }
                    if matches!(
                        agent.agent_status,
                        crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done
                    ) {
                        // Never mark Done from a reconcile directly: the settle
                        // window applies here too. An entry from an idle event has
                        // no sequence yet; record the one seen now and start its
                        // window here, since what came before it is unknown.
                        let entry = self
                            .pending_done
                            .entry(task.id)
                            .or_insert((None, Instant::now()));
                        if entry.0.is_none() {
                            *entry = (Some(agent.state_change_seq), Instant::now());
                        }
                        continue;
                    }
                    self.apply(task, &observed_from(agent));
                }
            }
        }
        self.find_orphans(&agents)?;
        self.refresh_live();
        for id in blocked {
            self.answer_limit_picker(id).await?;
        }
        self.auto_answer().await?;
        Ok(adopted)
    }

    /// Claude's limit picker: read the pane of task `id`, whose agent is
    /// `blocked`, once a blocked spell (`picker_read`), and when it ends on
    /// the picker (`limit::limit_picker`), pick "Stop and wait for limit to
    /// reset" by its text and handle the limit as an agent that stopped on
    /// it (`wait_on_limit`). The other options spend money or change the
    /// plan, which stays a person's call, so a picker without that option,
    /// or in words pastor does not know, gets no key: the task stays
    /// `blocked` with an error saying what it looks like. A task blocked on
    /// its startup (`prompt_pending`, see `auto_trust`), one its agent
    /// ended, and any where no limits are kept are not read.
    async fn answer_limit_picker(&mut self, id: i64) -> anyhow::Result<()> {
        if self.settings.limits.is_none() || self.picker_read.contains(&id) {
            return Ok(());
        }
        let Ok(Some(task)) = self.store.get_task(id) else {
            return Ok(());
        };
        if task.state != TaskState::Blocked || task.prompt_pending || task.ended {
            return Ok(());
        }
        let (Some(pane), Some(target)) = (task.pane_id.clone(), task.agent_name.clone()) else {
            return Ok(());
        };
        let timeout = self.settings.request_timeout;
        let text = match tokio::time::timeout(timeout, self.connector.agent_read(&target, 100))
            .await
        {
            Ok(Ok(text)) => text,
            Ok(Err(err)) if err.is_transport() => return Err(err.into()),
            Ok(Err(err)) => {
                tracing::warn!(machine = %self.name, task = %task.display_id(), %err, "read pane for a limit picker");
                self.picker_read.insert(id);
                return Ok(());
            }
            Err(_) => return Err(TimedOut("agent.read", timeout).into()),
        };
        self.picker_read.insert(id);
        let kind = self.settings.agents.kind(&task.spec.agent);
        // A login screen, which some agents show blocked: the agent is
        // signed out, not asking.
        let markers = self.settings.agents.login_markers(&task.spec.agent);
        if let Some(out) = crate::limit::signed_out_in(kind, &text, &markers) {
            return self.wait_on_limit(task, &out, false, &text).await;
        }
        let Some(picker) = crate::limit::limit_picker(kind, &text, Utc::now()) else {
            return Ok(());
        };
        let Some(keys) = picker.stop_keys() else {
            let message = format!(
                "looks like a usage limit picker with no \"{}\" ({}); nothing was pressed",
                crate::limit::STOP_AND_WAIT,
                picker.listed()
            );
            tracing::warn!(machine = %self.name, task = %task.display_id(), "{message}");
            let written = write_task(&self.store, task, |t| {
                let blocked = t.state == TaskState::Blocked;
                if blocked {
                    t.error = Some(message.clone());
                }
                blocked
            })?;
            if written.is_some() {
                self.refresh_live();
            }
            return Ok(());
        };
        tokio::time::timeout(timeout, self.connector.pane_send_keys(&pane, &keys))
            .await
            .map_err(|_| TimedOut("pane.send_keys", timeout))??;
        tracing::info!(machine = %self.name, task = %task.display_id(), "answered the limit picker: stop and wait");
        self.emit_with(
            "task.input",
            Some(id),
            Some(serde_json::json!({"keys": keys, "limit_picker": true})),
        );
        self.wait_on_limit(task, &picker.limit, false, &text).await
    }

    /// Answer the startup dialog of every task blocked while starting
    /// (`Blocked` with its prompt pending): read its pane once, take the
    /// first of its agent's dialogs (`Agents::dialogs`) whose marker shows
    /// in the bottom prompt, skip it if this task was already answered for
    /// it or it needs a saved trust the task's repo lacks, press its keys,
    /// and record the answer (`claim_answered`, once per task and dialog
    /// across restarts). One dialog per pass: the next reconcile or status
    /// reads the pane again and finds what follows. The trust row also
    /// claims `trust_sent` and emits `task.trusted`, as before, so pull
    /// machines and hooks see no change; every other row emits
    /// `task.answered`. A send that fails claims nothing, so a later pass
    /// tries again; the actor is the only writer of the claims for its
    /// tasks, so checking before the send and claiming after it cannot
    /// race. A `pick` not on screen presses nothing. A task no row matches,
    /// or still blocked after its rows, waits for a person. The pending
    /// prompt goes in when the agent reports it is no longer blocked
    /// (`deliver_pending_prompt`). A task inside its settle window after
    /// an answer (its own, or a person's) is not read: Claude is still
    /// replacing the dialog, and the window's tick reads it next.
    async fn auto_answer(&mut self) -> anyhow::Result<()> {
        for task in self.store.tasks_on_machine(&self.name)? {
            if task.state != TaskState::Blocked || !task.prompt_pending {
                continue;
            }
            if self
                .dialog_answered
                .get(&task.id)
                .is_some_and(|at| at.elapsed() < self.settings.settle)
            {
                continue;
            }
            let Some(pane) = &task.pane_id else { continue };
            let dialogs = self.settings.agents.dialogs(&task.spec.agent);
            if dialogs.is_empty() {
                continue;
            }
            let timeout = self.settings.request_timeout;
            let target = task.agent_name.as_deref().unwrap_or(pane);
            let screen = tokio::time::timeout(timeout, self.connector.agent_read(target, 100))
                .await
                .map_err(|_| TimedOut("agent.read", timeout))??;
            let Some((dialog, keys)) = self.pick_dialog(&task, &screen, false)? else {
                continue;
            };
            self.press_dialog(&task, pane, &dialog, keys).await?;
        }
        Ok(())
    }

    /// The first of `task`'s agent's dialogs that shows in `screen`, was
    /// not answered for the task, and applies, with the keys that answer
    /// it. A row with no marker (trust without one) shows on every screen,
    /// and an earlier dialog's text can linger in the read, so a row that
    /// is done or does not apply must not hide the ones after it. A `pick`
    /// not on screen gives nothing: the dialog is left for a person.
    /// `idle` is a screen read from an agent that stopped (`pane_end`), not
    /// one blocked while starting: the trust dialog and rows with no marker
    /// are not looked for there.
    fn pick_dialog(
        &self,
        task: &Task,
        screen: &str,
        idle: bool,
    ) -> anyhow::Result<Option<(crate::config::Dialog, Vec<String>)>> {
        let repo = task.spec.trust_dir();
        let mut dialog = None;
        for d in self.settings.agents.dialogs(&task.spec.agent) {
            if idle && (d.needs_trust || d.marker.is_none()) {
                continue;
            }
            let shows = d
                .marker
                .as_deref()
                .is_none_or(|m| crate::config::shows_trust_marker(screen, m));
            if !shows || self.store.answered(task.id, &d.name)? {
                continue;
            }
            let trusted = match repo {
                Some(r) => self.store.is_trusted(&self.name, r)?,
                None => false,
            };
            if d.needs_trust && !trusted {
                continue;
            }
            dialog = Some(d);
            break;
        }
        let Some(dialog) = dialog else {
            tracing::debug!(machine = %self.name, task = %task.display_id(), "no dialog to answer on this screen: one pastor does not know, or one that waits for a person");
            return Ok(None);
        };
        let keys = match &dialog.answer {
            crate::config::Answer::Keys(keys) => keys.clone(),
            crate::config::Answer::Pick(want) => {
                let found = crate::picker::at_bottom(screen).and_then(|f| f.picker.keys_to(want));
                let Some(keys) = found else {
                    tracing::warn!(machine = %self.name, task = %task.display_id(), dialog = %dialog.name, pick = %want, "the dialog shows no such option; left for a person");
                    return Ok(None);
                };
                keys
            }
        };
        Ok(Some((dialog, keys)))
    }

    /// The name of a dialog of `task`'s agent (not the trust one) whose
    /// marker shows in the bottom prompt of `screen`, answered or not.
    fn dialog_up(&self, task: &Task, screen: &str) -> Option<String> {
        self.settings
            .agents
            .dialogs(&task.spec.agent)
            .into_iter()
            .filter(|d| !d.needs_trust)
            .find(|d| {
                d.marker
                    .as_deref()
                    .is_some_and(|m| crate::config::shows_trust_marker(screen, m))
            })
            .map(|d| d.name)
    }

    /// Press `keys` in `pane` to answer `dialog` for `task`, and record the
    /// answer (`claim_answered`, once per task and dialog across restarts).
    /// The trust row also claims `trust_sent` and emits `task.trusted`;
    /// every other row emits `task.answered`.
    async fn press_dialog(
        &mut self,
        task: &Task,
        pane: &str,
        dialog: &crate::config::Dialog,
        keys: Vec<String>,
    ) -> anyhow::Result<()> {
        let timeout = self.settings.request_timeout;
        tokio::time::timeout(timeout, self.connector.pane_send_keys(pane, &keys))
            .await
            .map_err(|_| TimedOut("pane.send_keys", timeout))??;
        self.dialog_answered.insert(task.id, Instant::now());
        if !self.store.claim_answered(task.id, &dialog.name, &keys)? {
            return Ok(());
        }
        if dialog.needs_trust {
            self.store.claim_trust_sent(task.id, &keys)?;
            tracing::info!(machine = %self.name, task = %task.display_id(), repo = task.spec.trust_dir().unwrap_or(""), "answered the trust prompt of a trusted repo");
            self.emit_with(
                "task.trusted",
                Some(task.id),
                Some(serde_json::json!({"keys": keys})),
            );
        } else {
            tracing::info!(machine = %self.name, task = %task.display_id(), dialog = %dialog.name, "answered a startup dialog");
            self.emit_with(
                "task.answered",
                Some(task.id),
                Some(serde_json::json!({"dialog": dialog.name, "keys": keys})),
            );
        }
        Ok(())
    }

    /// Agents named `t-<id>` that no pane-owning task on this machine owns.
    /// Read after the reconcile pass above, so a task it just failed or closed
    /// counts as not owning its agent any more.
    fn find_orphans(&mut self, agents: &[AgentInfo]) -> anyhow::Result<()> {
        let found = orphan_agents(agents, &self.store.tasks_on_machine(&self.name)?);
        for (name, pane) in &found {
            if !self.orphans.iter().any(|(n, _)| n == name) {
                tracing::warn!(machine = %self.name, agent = %name, %pane, "orphaned agent: no open task owns it; `pastor task close` closes it, and auto-close once it stops for close_failed_after");
            }
        }
        self.orphans = found;
        Ok(())
    }

    /// A closed pane is no failed task's any more: herdr hands its id out
    /// again, and `auto_close_stopped` closes a failed task's recorded pane
    /// once it looks stopped, as an empty new pane would. The row keeps its
    /// state and worktree for `task retry`. An exited agent's pane is kept:
    /// it is still open, and still the task's to close.
    fn release_failed_pane(&mut self, pane_id: &str) -> anyhow::Result<()> {
        let failed = self.store.list_tasks(&TaskFilter {
            machine: Some(self.name.clone()),
            states: Some(vec![TaskState::Failed]),
            ..Default::default()
        })?;
        for t in failed
            .into_iter()
            .filter(|t| t.pane_id.as_deref() == Some(pane_id))
        {
            write_task(&self.store, t, |t| {
                let ours = t.state == TaskState::Failed && t.pane_id.as_deref() == Some(pane_id);
                if ours {
                    t.pane_id = None;
                }
                ours
            })?;
        }
        Ok(())
    }

    /// Is `pane` still held, by `agents`, by the stopped occupant its
    /// auto-close clock (`stopped_since`) was started for.
    fn still_stopped(&self, agents: &[AgentInfo], pane: &str) -> bool {
        stopped_in(agents, pane)
            && self
                .stopped_since
                .get(pane)
                .is_some_and(|(who, _)| *who == occupant(agents, pane))
    }

    /// Drop the orphan in `pane_id`, if there is one. Returns whether it did.
    fn forget_orphan(&mut self, pane_id: &str) -> bool {
        let before = self.orphans.len();
        self.orphans.retain(|(_, p)| p != pane_id);
        self.stopped_since.remove(pane_id);
        self.orphans.len() != before
    }
}

/// `update_task` with one retry. On `Conflict` (the row was written since it
/// was read, e.g. by `pastor task close` or `retry`), re-read it and apply
/// `change` to the fresh copy. `change` returns `false` when the row no
/// longer wants the change; then nothing is written. Returns the row as
/// written, or `None` when nothing was. A second conflict, or any other
/// store error, is returned.
///
/// The fresh copy is the row as it is now: `change` must set every field it
/// relies on rather than expect the old copy's values carried over.
fn write_task(
    store: &Store,
    mut task: Task,
    change: impl Fn(&mut Task) -> bool,
) -> anyhow::Result<Option<Task>> {
    if !change(&mut task) {
        return Ok(None);
    }
    match store.update_task(&mut task) {
        Ok(()) => return Ok(Some(task)),
        Err(err) if err.downcast_ref::<crate::store::Conflict>().is_none() => return Err(err),
        Err(_) => {}
    }
    let Some(mut fresh) = store.get_task(task.id)? else {
        return Ok(None);
    };
    if !change(&mut fresh) {
        return Ok(None);
    }
    store.update_task(&mut fresh)?;
    Ok(Some(fresh))
}

/// What a task that stopped on `row` does next (`task::NextRound`), and why
/// it waits when it could have moved on. With no fallback list it waits
/// for its own model. With one, a reset known and within `wait_under` is
/// waited for, list or not (a short wait keeps the model and the
/// conversation); any other moves it to its next model. `wait_under = 0`
/// never waits.
fn next_after_limit(
    t: &Task,
    row: &crate::limit::AccountLimit,
    limits: &crate::config::LimitsConfig,
    now: DateTime<Utc>,
) -> (crate::task::NextRound, Option<String>) {
    use crate::task::NextRound;
    if t.spec.agent_source.is_none() || t.fallback().is_empty() {
        return (NextRound::Own, None);
    }
    let wait_under = limits.wait_under_duration();
    if let Some(until) = row.until
        && !wait_under.is_zero()
    {
        let left = (until - now).to_std().unwrap_or_default();
        if left <= wait_under {
            let mins = left.as_secs().div_ceil(60);
            return (
                NextRound::Own,
                Some(format!(
                    "reset in {mins}m, under wait_under {}",
                    limits.wait_under.trim()
                )),
            );
        }
    }
    (NextRound::Switch, None)
}

/// Make `t` a task waiting on `row` (`TaskState::Waiting`): no pane or
/// workspace, pinned to its machine until `row.retry_at`, its error saying
/// which account and until when. `fresh_session` drops its session.
fn into_waiting(
    t: &mut Task,
    row: &crate::limit::AccountLimit,
    fresh_session: bool,
    now: DateTime<Utc>,
) {
    t.state = TaskState::Waiting;
    t.pane_id = None;
    t.workspace_id = None;
    t.prompt_pending = false;
    t.activity_seen = false;
    t.finished_at = None;
    t.waiting_until = Some(row.retry_at);
    t.error = Some(row.note(now));
    if fresh_session {
        t.spec.session_id = None;
    }
}

/// What `agent.list` says about an agent, as a state machine observation.
fn observed_from(agent: &AgentInfo) -> Observed {
    Observed::Status {
        status: agent.agent_status,
        state_change_seq: Some(agent.state_change_seq),
        completion_seq: agent.completion_seq,
    }
}

#[cfg(all(test, feature = "fake-herdr"))]
mod tests {
    /// A retry drops `spec.branch` and reopens the checkout of the task it
    /// retries; the note must name that checkout's branch, where the work is.
    #[test]
    fn a_kept_worktree_note_names_the_checkout_branch() {
        let store = Store::open_in_memory().unwrap();
        let mut t = new_task(&store);
        t.spec.branch = None;
        t.spec.checkout = Some(Box::new(crate::task::Checkout {
            branch: "pastor/t-4".into(),
            path: "/w/t-4".into(),
            already_open: false,
        }));
        let note = worktree_kept_note(&t, "t-9", "commits on no remote");
        assert!(note.contains("on branch pastor/t-4"), "{note}");
    }

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::herdr::fake::PaneInput;
    use crate::herdr::{AgentStatus, ConnectError, ConnectFuture, Connection};
    use crate::store::NewTask;
    use crate::task::{DispatchSpec, Place};

    fn settings() -> MachineSettings {
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
            version_every: Duration::from_millis(200),
            room_every: Duration::from_millis(200),
            agents: Default::default(),
            head_address: None,
            cut_off_from_head: false,
            limits: None,
            own_policy: None,
            harness_pins: Default::default(),
            harnesses: Default::default(),
            bundle_gate: Default::default(),
            workdir_gate: Default::default(),
            env: Default::default(),
        }
    }

    /// `agent_ready_timeout` has to stay under `request_timeout`, which bounds a
    /// whole dispatch: otherwise the readiness wait is cut short by the outer
    /// timeout and reported as a wedged machine instead of an agent that never
    /// came up. Pinned for the shipped defaults and for the test settings above.
    #[test]
    fn ready_timeout_stays_under_the_request_timeout() {
        for s in [MachineSettings::default(), settings()] {
            assert!(
                s.agent_ready_timeout < s.request_timeout,
                "{:?} >= {:?}",
                s.agent_ready_timeout,
                s.request_timeout
            );
        }
    }

    /// `settings()` with another settle window, for the tests that wait out a
    /// part of the window and assert it has *not* fired yet.
    fn settings_with_settle(settle: Duration) -> MachineSettings {
        MachineSettings {
            settle,
            ..settings()
        }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec {
            now: false,
            agent: "claude".into(),
            agent_args: vec![],
            allow: vec![],
            deny: vec![],
            repo: None,
            worktree: false,
            branch: None,
            machine: None,
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
        }
    }

    fn new_task(store: &Store) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: spec(),
                flock: "default".into(),
            })
            .unwrap()
    }

    /// Polls on tokio's clock. The tests here start with it paused
    /// (`start_paused`), so a sleep costs no wall time: the clock jumps to the
    /// next timer once every task is waiting, and each timer of the actor
    /// fires in order on the way. A sleep followed by "nothing happened" has
    /// therefore let the actor do all it would in that time, on any runner.
    /// What the actor measures in calendar time (a task's timeout, the
    /// auto-close grace) does not move with it: see `start_an_hour_ago` and
    /// `finished_an_hour_ago`.
    async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn spawn(
        fake: &FakeHerdr,
        store: &Arc<Store>,
    ) -> (MachineHandle, broadcast::Receiver<PastorEvent>) {
        spawn_with_settings(fake, store, settings())
    }

    fn spawn_with_settings(
        fake: &FakeHerdr,
        store: &Arc<Store>,
        settings: MachineSettings,
    ) -> (MachineHandle, broadcast::Receiver<PastorEvent>) {
        let (events, rx) = broadcast::channel(64);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(fake.clone()),
            store.clone(),
            settings,
            events,
        );
        (h, rx)
    }

    fn state_of(store: &Store, id: i64) -> TaskState {
        store.get_task(id).unwrap().unwrap().state
    }

    /// Poisons the status lock the way a panicking request would: a thread
    /// panics while it holds the write guard.
    fn poison_status(h: &MachineHandle) {
        let status = h.status.clone();
        let r = std::thread::spawn(move || {
            let _s = status.write().unwrap();
            panic!("poison the status lock");
        })
        .join();
        assert!(r.is_err());
        assert!(h.status.is_poisoned());
    }

    /// A panic while the status lock is held must not break the handle: the
    /// status is plain values, so later reads and writes take the guard back.
    #[test]
    fn the_handle_works_after_a_panic_poisoned_the_status_lock() {
        let h = pull_machine("m".into(), 2, vec![]);
        poison_status(&h);
        let h = h.with_slots(3, 1);
        let s = h.snapshot();
        assert_eq!((s.job_slots, s.burst), (3, 1));
    }

    /// The actor writes the status on every connect and poll. With the lock
    /// poisoned it must still connect and report, not panic and leave the
    /// machine `connecting` until restart.
    #[tokio::test]
    async fn the_actor_works_after_a_panic_poisoned_the_status_lock() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        // The test runtime has one thread, so the actor has not run yet.
        poison_status(&h);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        assert!(!h.actor_finished(), "the actor survived the poisoned lock");
        let t = new_task(&store);
        h.dispatch(t.id).await.unwrap();
        wait_for("live count", || h.snapshot().live == 1).await;
    }

    /// The actor hands its machine's own profile (`own_policy`) to dispatch,
    /// so a profiled agy task there writes its rules into agy's settings.
    #[tokio::test]
    async fn the_actor_provisions_agy_with_its_own_profile() {
        let fake = FakeHerdr::new();
        fake.set_agy_settings(Some(crate::provision::agy::AgySettings {
            content: None,
            link: false,
        }));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                own_policy: Some(crate::config::agy::Policy {
                    deny: vec!["Bash(sudo:*)".into()],
                    ..Default::default()
                }),
                ..settings()
            },
        );
        let t = store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    agent: "agy".into(),
                    agent_source: Some(Box::new(crate::task::AgentSource {
                        profile: Some("develop".into()),
                        ..Default::default()
                    })),
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap();
        let _ = h.dispatch(t.id).await;
        let content = fake.agy_settings_now().unwrap().content.unwrap_or_default();
        assert!(content.contains("command(sudo)"), "{content}");
    }

    /// The actor hands its machine's env layers (`env`) to dispatch: the
    /// pane of a task gets `[defaults]`'s, the task's flock's and the
    /// machine's, each key over the one before.
    #[tokio::test]
    async fn the_actor_gives_its_agents_the_env_layers() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let layer = |kv: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                env: crate::config::EnvLayers {
                    defaults: layer(&[("A", "defaults"), ("B", "defaults")]),
                    flocks: [(
                        "default".to_string(),
                        layer(&[("B", "flock"), ("C", "flock")]),
                    )]
                    .into(),
                    machine: layer(&[("C", "machine")]),
                },
                ..settings()
            },
        );
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert_eq!(env["A"], "defaults");
        assert_eq!(env["B"], "flock");
        assert_eq!(env["C"], "machine");
    }

    /// A worktree task whose base could not be fetched starts anyway: the
    /// row carries the reason and a `task.fetch_failed` event says it; one
    /// whose fetch went fine says nothing.
    #[tokio::test]
    async fn a_failed_fetch_before_a_worktree_is_a_warning_and_an_event() {
        let task_for = |store: &Store| {
            store
                .insert_task(NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "hi".into(),
                    spec: DispatchSpec {
                        worktree: true,
                        repo: Some("/r/app".into()),
                        ..spec()
                    },
                    flock: "default".into(),
                })
                .unwrap()
        };
        for fails in [true, false] {
            let fake = FakeHerdr::new();
            if fails {
                fake.set_fetch(crate::herdr::Fetched::Failed(
                    "origin".into(),
                    "fatal: no route to host".into(),
                ));
            }
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, mut events) = spawn(&fake, &store);
            let t = task_for(&store);
            let t = h.dispatch(t.id).await.unwrap();
            assert_eq!(t.state, TaskState::Running, "{fails}");
            let mut found = Vec::new();
            while let Ok(ev) = events.try_recv() {
                if ev.kind == "task.fetch_failed" {
                    found.push(ev);
                }
            }
            if fails {
                let why = t.spec.fetch.warning.clone().unwrap();
                assert!(why.contains("no route to host"), "{why}");
                assert_eq!(found.len(), 1, "{found:?}");
                let detail = found[0].detail.clone().unwrap();
                assert_eq!(detail["repo"], "/r/app");
                assert_eq!(detail["why"], why);
                let row = store.get_task(t.id).unwrap().unwrap();
                assert_eq!(row.spec.fetch.warning.as_deref(), Some(why.as_str()));
            } else {
                assert!(found.is_empty(), "{found:?}");
                assert_eq!(t.spec.fetch.warning, None);
            }
        }
    }

    /// A flock reload stops a removed or replaced machine's actor. Afterwards
    /// the actor asks herdr nothing, the handle refuses new work, and the task
    /// it was tracking is left exactly as it was: agents are herdr's.
    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_the_actor_and_leaves_tasks_alone() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        h.dispatch(t.id).await.unwrap();
        assert_eq!(state_of(&store, t.id), TaskState::Running);

        assert_eq!(h.shutdown().await, ShutdownOutcome::Finished);
        // No waiting here: once `shutdown` returns the actor task has ended,
        // so it can neither write a row nor ask herdr anything.
        assert!(h.actor_finished(), "shutdown waits for the task to end");
        assert!(h.tx.is_closed());
        let sent = fake.requests().len();
        // `settings()` reconciles every 200ms: a live actor would have called
        // agent.list at least twice in this window.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            fake.requests().len(),
            sent,
            "a stopped actor asks herdr nothing"
        );

        let queued = new_task(&store);
        let err = h.dispatch(queued.id).await.unwrap_err();
        assert!(err.to_string().contains("is gone"), "{err}");
        assert_eq!(state_of(&store, queued.id), TaskState::Queued);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// An abort lands at the actor's next await, so an actor stuck inside a
    /// poll outlives it. `shutdown` says so instead of returning as if it had
    /// ended, and keeps the task so a later call can wait for it again: a
    /// reload must not spawn a replacement next to a live actor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_reports_an_actor_that_does_not_stop() {
        let fake = FakeHerdr::new();
        fake.wedge_connects(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("actor wedged in connect", || fake.wedged() == 1).await;

        assert_eq!(h.shutdown().await, ShutdownOutcome::StillRunning);
        assert!(!h.actor_finished());
        // A clone shares the task: it too sees an actor that has not ended.
        assert_eq!(h.clone().shutdown().await, ShutdownOutcome::StillRunning);

        fake.wedge_connects(false);
        assert_eq!(h.shutdown().await, ShutdownOutcome::Finished);
        assert!(h.actor_finished());
        assert!(h.tx.is_closed());
        assert_eq!(h.shutdown().await, ShutdownOutcome::Finished, "idempotent");
    }

    /// A request already queued on an actor that is then aborted but stays
    /// stuck in a poll must not wait for that actor: nothing will ever read
    /// it. It fails as soon as the shutdown starts, and so does one sent
    /// afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_on_a_stopped_actor_fails_promptly() {
        let fake = FakeHerdr::new();
        fake.wedge_connects(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        // Unwedged even when an assert fails, or the runtime never shuts down.
        struct Unwedge(FakeHerdr);
        impl Drop for Unwedge {
            fn drop(&mut self) {
                self.0.wedge_connects(false);
            }
        }
        let _unwedge = Unwedge(fake.clone());
        let (h, _events) = spawn(&fake, &store);
        wait_for("actor wedged in connect", || fake.wedged() == 1).await;

        let queued = tokio::spawn({
            let h = h.clone();
            async move { h.read(1, 10).await }
        });
        wait_for("read queued", || h.tx.capacity() < h.tx.max_capacity()).await;
        assert_eq!(h.shutdown().await, ShutdownOutcome::StillRunning);
        let err = tokio::time::timeout(Duration::from_secs(1), queued)
            .await
            .expect("a queued request fails once the actor is stopped")
            .unwrap()
            .unwrap_err();
        assert!(err.downcast_ref::<ActorStopped>().is_some(), "{err:#}");

        let err = tokio::time::timeout(Duration::from_secs(1), h.close(1, false))
            .await
            .expect("a request after the stop fails at once")
            .unwrap_err();
        assert!(err.downcast_ref::<ActorStopped>().is_some(), "{err:#}");
    }

    #[test]
    fn settings_compare_by_value() {
        assert_eq!(MachineSettings::default(), MachineSettings::default());
        assert_ne!(settings(), MachineSettings::default());
    }

    /// Someone else wrote the row (task close, retry) between our read and
    /// our write: the change is re-applied to the fresh row, keeping their
    /// fields.
    #[test]
    fn write_task_retries_a_conflict_once_on_the_fresh_row() {
        let store = Store::open_in_memory().unwrap();
        let t = new_task(&store);
        let stale = t.clone();
        let mut other = t.clone();
        other.error = Some("written by someone else".into());
        store.update_task(&mut other).unwrap();

        let written = write_task(&store, stale, |t| {
            t.state = TaskState::Stale;
            true
        })
        .unwrap()
        .expect("the fresh row still wanted the change");
        assert_eq!(written.state, TaskState::Stale);
        assert_eq!(written.error.as_deref(), Some("written by someone else"));
        assert_eq!(state_of(&store, t.id), TaskState::Stale);
    }

    /// A row that moved on to a state the change does not apply to is left
    /// alone: a task closed meanwhile is never reopened.
    #[test]
    fn write_task_leaves_a_row_that_moved_on() {
        let store = Store::open_in_memory().unwrap();
        let t = new_task(&store);
        let stale = t.clone();
        // What `pastor task close` does, outside the actor.
        let mut closed = t.clone();
        closed.state = TaskState::Closed;
        closed.finished_at = Some(Utc::now());
        store.update_task(&mut closed).unwrap();
        let written = write_task(&store, stale, |t| {
            let open = t.state != TaskState::Closed;
            if open {
                t.state = TaskState::Stale;
            }
            open
        })
        .unwrap();
        assert!(written.is_none());
        assert_eq!(state_of(&store, t.id), TaskState::Closed);
    }

    /// The retried write carries what the change set on the fresh row,
    /// `activity_seen` included, and stores it.
    #[test]
    fn write_task_stores_the_activity_flag_on_the_retry() {
        let store = Store::open_in_memory().unwrap();
        let mut t = new_task(&store);
        t.prompt_pending = true;
        store.update_task(&mut t).unwrap();
        let stale = t.clone();
        let mut other = t.clone();
        other.error = Some("written by someone else".into());
        store.update_task(&mut other).unwrap();

        let written = write_task(&store, stale, |t| {
            t.prompt_pending = false;
            t.activity_seen = true;
            true
        })
        .unwrap()
        .expect("the fresh row still wanted the change");
        assert!(written.activity_seen);
        let row = store.get_task(t.id).unwrap().unwrap();
        assert!(!row.prompt_pending);
        assert!(row.activity_seen);
    }

    /// A second conflict is not retried again: it is returned to the caller.
    #[test]
    fn write_task_returns_a_second_conflict() {
        let store = Store::open_in_memory().unwrap();
        let t = new_task(&store);
        let stale = t.clone();
        let mut other = t.clone();
        store.update_task(&mut other).unwrap();
        let calls = std::cell::Cell::new(0);
        let err = write_task(&store, stale, |t| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                // Someone writes again between the re-read and the retry.
                let mut again = store.get_task(t.id).unwrap().unwrap();
                store.update_task(&mut again).unwrap();
            }
            t.state = TaskState::Stale;
            true
        })
        .unwrap_err();
        assert!(err.downcast_ref::<crate::store::Conflict>().is_some());
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn dispatch_then_events_drive_state() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(500)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(h.snapshot().live, 1);
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let ev = loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            if ev.kind == "task.blocked" {
                break ev;
            }
            assert_eq!(
                ev.kind, "task.running",
                "only the dispatch event may precede task.blocked"
            );
        };
        assert_eq!(ev.task_id, Some(t.id));
        assert_eq!(
            ev.job.as_deref(),
            Some("run"),
            "task events carry the row's job"
        );

        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;

        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "done waits for the settle window"
        );
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().last_completion_seq,
            Some(fake.agents()[0].state_change_seq),
            "a done task's baseline moves to the change that finished it"
        );

        fake.close_pane(&pane);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        wait_for("live 0", || h.snapshot().live == 0).await;
        let _ = h.read(t.id, 10).await.unwrap_err();
    }

    /// The tests here pause tokio's clock, so the actor has to measure the
    /// settle window on that clock and not the wall's: else a test that waits
    /// out `settle` and asserts nothing happened would pass with no window
    /// ever over. Ten seconds of settle go by in no wall time at all.
    #[tokio::test(start_paused = true)]
    async fn the_settle_window_runs_on_the_paused_clock() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_secs(10);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;

        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle / 2).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "half a window is not settled"
        );
        tokio::time::sleep(settle).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Done,
            "settled once the window is over on tokio's clock"
        );
    }

    /// An agent that ends its turn on a question goes idle just like one that
    /// finished: pastor reads the pane and marks the task blocked, with the
    /// question as its error, and keeps it there until the agent moves. The
    /// same agent answered, working and idle again with a plain report, is
    /// done.
    #[tokio::test(start_paused = true)]
    async fn a_turn_that_ends_on_a_question_is_blocked_not_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(200);
        let (h, mut events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(
            &pane,
            "● Two ways to do this.\n\n  Should I keep the old flag?\n\n✻ Baked for 1m\n\n❯\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            row.error.as_deref(),
            Some("agent asked: Should I keep the old flag?")
        );
        assert_eq!(row.finished_at, None);
        let ev = loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            if ev.kind != "task.running" {
                break ev;
            }
        };
        assert_eq!(ev.kind, "task.blocked");
        assert_eq!(
            ev.detail,
            Some(serde_json::json!({"question": "Should I keep the old flag?"}))
        );

        // Idle events and reconciles at the same sequence leave it blocked.
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 4).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);

        // `task send` answered it: the agent works, reports and goes idle.
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        assert_eq!(store.get_task(t.id).unwrap().unwrap().error, None);
        fake.set_pane_text(&pane, "● Kept it. Pushed the branch.\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// A task whose agent goes idle with `DONE` alone as its last line is
    /// ended as if it had run `pastor task done`: done, `ended`, one
    /// `task.done`, and the message is the round's summary (the agent's, not
    /// the pane's tail). The same for agy, whose message is what sits above
    /// its input. A `DONE` that is not the last line ends nothing: a Claude
    /// task is done on the idle as before but its summary is the pane, and an
    /// agy task is blocked until it ends itself.
    #[tokio::test(start_paused = true)]
    async fn a_last_line_done_ends_the_task_with_that_message() {
        use crate::task::SummarySource;
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(200);
        let (h, mut events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let go = |agent: &'static str| {
            let (h, store) = (h.clone(), store.clone());
            async move {
                let t = h.dispatch(new_task_of(&store, agent).id).await.unwrap();
                (t.id, t.pane_id.clone().unwrap())
            }
        };

        // Claude: the last line of its last message.
        let (id, pane) = go("claude").await;
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(
            &pane,
            "● Pushed the branch and opened the PR.\n\n  DONE\n\n✻ Baked for 1m\n\n❯\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, id) == TaskState::Done).await;
        let row = store.get_task(id).unwrap().unwrap();
        assert!(row.ended, "ended as by `task done`");
        let round = row.summary.unwrap();
        assert_eq!(round.source, SummarySource::Agent);
        assert_eq!(round.text, "Pushed the branch and opened the PR.\nDONE");
        assert_eq!(count(&mut events, "task.done", id).len(), 1);

        // agy: above its input, and with no `task done` the pane waits for.
        let (id, pane) = go("agy").await;
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(
            &pane,
            "  Opened the PR.\n  DONE\n\n╭──────╮\n│ >    │\n╰──────╯\n ~/src/repo  gemini-3-pro\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, id) == TaskState::Done).await;
        let row = store.get_task(id).unwrap().unwrap();
        assert!(row.ended);
        assert_eq!(row.summary.unwrap().text, "Opened the PR.\nDONE");

        // Not the last line: a Claude task is done on the idle as it always
        // was, with the pane for its summary; an agy task has not ended.
        let (id, pane) = go("claude").await;
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● DONE\n\n  but the lint still fails.\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, id) == TaskState::Done).await;
        let row = store.get_task(id).unwrap().unwrap();
        assert!(!row.ended);
        assert_eq!(row.summary.unwrap().source, SummarySource::Pane);

        let (id, pane) = go("agy").await;
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "  DONE, almost.\n\n  > \n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("blocked", || state_of(&store, id) == TaskState::Blocked).await;
        assert!(!store.get_task(id).unwrap().unwrap().ended);
    }

    /// On a task that must say what it did, a bare `DONE` is no summary, and
    /// a `DONE` from the round before does not end a reopened task whose new
    /// turn drew no message (interrupted, or ended by an API error): the task
    /// fails as one that stopped without a summary.
    #[tokio::test(start_paused = true)]
    async fn a_required_task_is_not_ended_by_a_bare_or_an_old_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(150);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        // A bare DONE: nothing above it says what was done.
        let t = h.dispatch(new_required_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● DONE\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        assert!(!store.get_task(t.id).unwrap().unwrap().ended);

        // A message and DONE ends it; a later turn that draws no message
        // does not end it again with the round before's words.
        let t = h.dispatch(new_required_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● Opened the PR.\n\n  DONE\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(store.get_task(t.id).unwrap().unwrap().ended);
        tokio::time::sleep(settle * 3).await;
        let input = SendInput {
            text: Some("commit and push".into()),
            enter: true,
            ..Default::default()
        };
        h.send(t.id, input).await.unwrap();
        fake.set_pane_text(
            &pane,
            "● Opened the PR.\n\n  DONE\n\n❯ commit and push\n  ⎿  API Error: 500\n\n❯\n",
        );
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("activity recorded", || {
            store.get_task(t.id).unwrap().unwrap().activity_seen
        })
        .await;
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert!(!row.ended);
        assert_eq!(
            row.error.as_deref(),
            Some(crate::task::STOPPED_WITHOUT_SUMMARY)
        );
    }

    /// An agent that stops to ask or to wait is not ended by a `DONE` that is
    /// not the end of its message; one that ends on a question is blocked as
    /// before, even if an earlier message said `DONE`.
    #[tokio::test(start_paused = true)]
    async fn a_question_or_a_wait_is_not_closed_by_an_earlier_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(200);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(
            &pane,
            "● DONE\n\n❯ one more thing\n\n● Two ways to do it.\n\n  Should I keep the old flag?\n\n❯\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert!(!row.ended);
        assert_eq!(
            row.error.as_deref(),
            Some("agent asked: Should I keep the old flag?")
        );
    }

    /// `settings_with_settle` on a head, which keeps the limits.
    fn limits_kept(settle: Duration) -> MachineSettings {
        MachineSettings {
            limits: Some(Default::default()),
            ..settings_with_settle(settle)
        }
    }

    /// A Claude task that ends has its session's usage read from the
    /// machine, under the agent's `CLAUDE_CONFIG_DIR`, and kept with it.
    #[tokio::test(start_paused = true)]
    async fn a_claude_task_that_ends_keeps_its_sessions_usage() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let mut agents = crate::config::Agents::default();
        agents.0.insert(
            "claude".into(),
            crate::config::AgentDef {
                env: [("CLAUDE_CONFIG_DIR".into(), "~/.claude-alt".into())].into(),
                ..Default::default()
            },
        );
        let settings = MachineSettings {
            agents,
            ..settings_with_settle(settle)
        };
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let session = t.spec.session_id.clone().expect("a Claude session");
        let usage = crate::usage::TaskUsage {
            model: Some("claude-opus-4-5".into()),
            api_calls: 4,
            input: 10,
            cache_write: 2_000,
            cache_read: 90_000,
            output: 700,
            read_at: Utc::now(),
        };
        fake.set_usage(Some("~/.claude-alt"), &session, usage.clone());
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        assert_eq!(store.get_task(t.id).unwrap().unwrap().usage, None);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        wait_for("usage", || {
            store.get_task(t.id).unwrap().unwrap().usage.is_some()
        })
        .await;
        assert_eq!(store.get_task(t.id).unwrap().unwrap().usage, Some(usage));
    }

    /// The bug: an agent that stops on "You've hit your limit" is idle
    /// like one that finished. Its task goes `waiting`, not done: the limit
    /// in the table, `esc` and its pane closed, its checkout kept, pinned
    /// here until the reset. Resumed, it goes back to its session with the
    /// limit's resume line.
    #[tokio::test(start_paused = true)]
    async fn an_agent_idle_on_a_usage_limit_waits_and_resumes_its_session() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, mut events) = spawn_with_settings(&fake, &store, limits_kept(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let session = t.spec.session_id.clone().expect("a Claude session");
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(
            &pane,
            "❯ hi\n\n● Half of it is done.\n\n● You've hit your limit · resets 3am (UTC)\n\n────────\n❯ \n────────\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.pane_id, None);
        assert_eq!(row.workspace_id, None);
        assert_eq!(row.machine.as_deref(), Some("m"));
        assert_eq!(row.spec.session_id.as_deref(), Some(session.as_str()));
        let limits = store.limits().unwrap();
        assert_eq!(limits.len(), 1, "{limits:?}");
        assert_eq!(limits[0].account, "m/claude");
        assert_eq!(limits[0].task_id, Some(t.id));
        assert_eq!(row.waiting_until, Some(limits[0].retry_at));
        let error = row.error.clone().unwrap();
        assert!(error.starts_with("m/claude exhausted until "), "{error}");
        assert_eq!(
            fake.pane_input(&pane),
            vec![crate::herdr::fake::PaneInput::Keys(vec!["esc".into()])]
        );
        assert!(
            calls(&fake, "pane.close")
                .iter()
                .any(|c| c["pane_id"] == pane.as_str())
        );
        let mut kinds = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if ev.task_id == Some(t.id) {
                kinds.push((ev.kind, ev.detail));
            }
        }
        let has = |k: &str| kinds.iter().any(|(kind, _)| kind == k);
        assert!(has("agent.exhausted") && has("task.limited"), "{kinds:?}");
        assert!(!has("task.done"), "{kinds:?}");
        let (_, waiting) = kinds.iter().find(|(k, _)| k == "task.waiting").unwrap();
        let waiting = waiting.as_ref().unwrap();
        assert_eq!(waiting["why"], "no_fallback");
        assert_eq!(
            row.spec.rounds.stop.as_ref().map(|s| s.then),
            Some(crate::task::NextRound::Own),
            "no list: it waits for its own model"
        );
        assert!(
            waiting["shown"].as_str().unwrap().starts_with("waiting "),
            "{waiting}"
        );

        // Nothing seen on the machine moves it meanwhile.
        tokio::time::sleep(settle * 4).await;
        assert_eq!(state_of(&store, t.id), TaskState::Waiting);

        let t = h.resume(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.waiting_until, None);
        let start = calls(&fake, "agent.start").pop().unwrap();
        assert!(
            start["args"].to_string().contains(&session),
            "{}",
            start["args"]
        );
        let prompt = calls(&fake, "agent.prompt").pop().unwrap();
        assert_eq!(prompt["text"], crate::task::LIMIT_RESUME_PROMPT);
    }

    /// An agy task that stops on its quota is not done either: it waits
    /// for the reset its message names. agy has no session to resume, so
    /// it starts again from its prompt, with the handover, at that time.
    #[tokio::test(start_paused = true)]
    async fn an_agy_task_at_its_quota_waits_and_starts_again_with_the_handover() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, mut events) = spawn_with_settings(&fake, &store, limits_kept(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task_of(&store, "agy").id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        assert_eq!(t.spec.session_id, None);
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        let before = Utc::now();
        fake.set_pane_text(
            &pane,
            "  Half of the review is written.\n\n\
             \u{20} RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your\n\
             \u{20} subscription to increase your limits. Resets in 4h21m30s.\n\n\
             ╭────────╮\n│ >      │\n╰────────╯\n ~/src/repo  gemini-3-pro\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.pane_id, None);
        let limits = store.limits().unwrap();
        assert_eq!(limits.len(), 1, "{limits:?}");
        assert_eq!(limits[0].account, "m/agy");
        assert!(limits[0].hard);
        let until = limits[0].until.expect("the reset agy named");
        let wait = until - before;
        assert!(
            (wait - chrono::Duration::seconds(4 * 3600 + 21 * 60 + 30))
                .num_seconds()
                .abs()
                <= 2,
            "{wait}"
        );
        assert_eq!(row.waiting_until, Some(limits[0].retry_at));
        let mut kinds = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if ev.task_id == Some(t.id) {
                kinds.push(ev.kind);
            }
        }
        assert!(kinds.iter().any(|k| k == "task.limited"), "{kinds:?}");
        assert!(!kinds.iter().any(|k| k == "task.done"), "{kinds:?}");

        let t = h.resume(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.spec.session_id, None);
        let start = calls(&fake, "agent.start").pop().unwrap();
        assert_eq!(start["kind"], "agy");
        let prompt = calls(&fake, "agent.prompt").pop().unwrap();
        let text = prompt["text"].as_str().unwrap();
        assert!(text.starts_with("hi"), "{text}");
        assert!(text.ends_with(crate::task::LIMIT_HANDOVER), "{text}");
    }

    /// `confirm_pending_done` sees the same `agent.list` reply
    /// `capture_agent_session` reads a conversation id from: a hard quota
    /// closes the pane in the very same pass, before a reconcile (due only
    /// every `reconcile_every`) would otherwise have had a chance to record
    /// it. The id must not be lost to that race.
    #[tokio::test(start_paused = true)]
    async fn an_agy_session_is_captured_before_a_hard_quota_closes_its_pane() {
        use crate::herdr::{AgentSessionInfo, AgentSessionRefKind};

        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        // No reconcile inside the test, so only the settle check can record it.
        let settings = MachineSettings {
            reconcile_every: Duration::from_secs(3600),
            ..limits_kept(Duration::from_millis(100))
        };
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task_of(&store, "agy").id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        const CONVERSATION: &str = "9f1e2d3c-4b5a-6978-8f90-123456789abc";
        fake.set_agent_session(
            &pane,
            Some(AgentSessionInfo {
                agent: "agy".into(),
                kind: AgentSessionRefKind::Id,
                source: "agy-hook".into(),
                value: CONVERSATION.into(),
            }),
        );
        fake.set_pane_text(
            &pane,
            "  Half of the review is written.\n\n\
             \u{20} RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your\n\
             \u{20} subscription to increase your limits. Resets in 4h21m30s.\n\n\
             ╭────────╮\n│ >      │\n╰────────╯\n ~/src/repo  gemini-3-pro\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.pane_id, None, "its pane closed on the hard quota");
        assert_eq!(
            row.spec.session_id.as_deref(),
            Some(CONVERSATION),
            "the conversation id is kept despite the pane closing before any reconcile ran"
        );
    }

    /// An opencode task that fell back to GPT and ran out there is not
    /// done either: out of credit, it waits `retry_after_no_credit`. opencode
    /// has no session to go back to, so at its time it starts again from
    /// its prompt with the handover.
    #[tokio::test(start_paused = true)]
    async fn an_opencode_task_out_of_quota_waits_and_starts_again_with_the_handover() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, _events) = spawn_with_settings(&fake, &store, limits_kept(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h
            .dispatch(new_task_of(&store, "opencode").id)
            .await
            .unwrap();
        let pane = t.pane_id.clone().unwrap();
        assert_eq!(t.spec.session_id, None, "opencode keeps no session");
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        let seen = Utc::now();
        fake.set_pane_text(
            &pane,
            "  ┃\n  ┃  hi\n  ┃\n\n  ┃\n  ┃  You exceeded your current quota, please check your plan and billing details.\n  ┃\n\n     ▣  Build · GPT-4.1\n\n  ┃\n  ┃\n  ┃  Build · GPT-4.1 OpenAI\n  ╹▀▀▀▀▀▀▀▀\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.pane_id, None);
        let limits = store.limits().unwrap();
        assert_eq!(limits.len(), 1, "{limits:?}");
        assert_eq!(limits[0].account, "m/opencode");
        assert!(limits[0].hard && limits[0].no_credit, "{limits:?}");
        let wait = crate::config::LimitsConfig::default().retry_after_no_credit_duration();
        let wait = chrono::Duration::from_std(wait).unwrap();
        let retry = limits[0].retry_at;
        assert!(
            retry >= seen + wait && retry <= Utc::now() + wait,
            "{retry} is not {wait} after the limit"
        );
        assert_eq!(row.waiting_until, Some(retry));

        tokio::time::sleep(settle * 4).await;
        assert_eq!(state_of(&store, t.id), TaskState::Waiting);

        let t = h.resume(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.waiting_until, None);
        let start = calls(&fake, "agent.start").pop().unwrap();
        assert_eq!(start["kind"], "opencode");
        let prompt = calls(&fake, "agent.prompt").pop().unwrap();
        let prompt = prompt["text"].as_str().unwrap();
        assert!(prompt.starts_with("hi"), "{prompt}");
        assert!(prompt.ends_with(crate::task::LIMIT_HANDOVER), "{prompt}");
    }

    /// A task with a fallback list whose agent stops on a limit that resets
    /// `in_mins` from now, and what it left: its row once `waiting`, and
    /// the task's events.
    async fn limited_with_fallback(
        in_mins: i64,
    ) -> (Task, Vec<(String, Option<serde_json::Value>)>) {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, mut events) = spawn_with_settings(&fake, &store, limits_kept(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let mut t = new_task(&store);
        t.spec.agent_source = Some(Box::new(crate::task::AgentSource {
            model: Some("opus".into()),
            fallback: vec!["sonnet".into(), "gpt".into()],
            ..Default::default()
        }));
        store.update_task(&mut t).unwrap();
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        let reset = (Utc::now() + chrono::Duration::minutes(in_mins)).timestamp();
        fake.set_pane_text(
            &pane,
            &format!("❯ hi\n\n● Half of it is done.\n\n● Claude AI usage limit reached|{reset}\n\n────────\n❯ \n────────\n"),
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
        let mut kinds = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if ev.task_id == Some(t.id) {
                kinds.push((ev.kind, ev.detail));
            }
        }
        (store.get_task(t.id).unwrap().unwrap(), kinds)
    }

    /// With a fallback list and a reset two hours off, the task is left for
    /// the next dispatch pass to move to its next model, at once, with the
    /// end of its pane; it says nothing of waiting.
    #[tokio::test(start_paused = true)]
    async fn a_far_reset_with_a_fallback_list_moves_the_task_on() {
        let (row, kinds) = limited_with_fallback(120).await;
        let stop = row.spec.rounds.stop.clone().expect("a stop");
        assert_eq!(stop.then, crate::task::NextRound::Switch);
        assert_eq!(
            (stop.agent.as_str(), stop.model.as_deref()),
            ("claude", Some("opus"))
        );
        assert_eq!(stop.why, "limit");
        assert!(stop.tail.contains("Half of it is done."), "{}", stop.tail);
        assert!(row.waiting_until.unwrap() <= Utc::now());
        assert!(row.spec.session_id.is_some(), "its session is kept");
        let has = |k: &str| kinds.iter().any(|(kind, _)| kind == k);
        assert!(has("task.limited"), "{kinds:?}");
        assert!(!has("task.waiting"), "{kinds:?}");
    }

    /// A reset ten minutes off is waited for, list or not, and the task
    /// says why it did not move on.
    #[tokio::test(start_paused = true)]
    async fn a_near_reset_is_waited_for_even_with_a_fallback_list() {
        let (row, kinds) = limited_with_fallback(10).await;
        let stop = row.spec.rounds.stop.clone().expect("a stop");
        assert_eq!(stop.then, crate::task::NextRound::Own);
        let waited = stop.waited.clone().unwrap();
        assert!(
            waited.starts_with("reset in ") && waited.ends_with("m, under wait_under 30m"),
            "{waited}"
        );
        assert!(row.waiting_until.unwrap() > Utc::now());
        let (_, waiting) = kinds.iter().find(|(k, _)| k == "task.waiting").unwrap();
        let waiting = waiting.as_ref().unwrap();
        assert_eq!(waiting["why"], "reset_soon");
        assert_eq!(
            waiting["until"],
            serde_json::json!(row.waiting_until.unwrap())
        );
    }

    /// `limits_kept` with short limits retried after 1s, 2s, then 3s.
    fn short_limits(rate_retries: u32) -> MachineSettings {
        MachineSettings {
            limits: Some(crate::config::LimitsConfig {
                rate_retries,
                rate_backoff: vec!["1s".into(), "2s".into(), "3s".into()],
                ..Default::default()
            }),
            ..settings_with_settle(Duration::from_millis(100))
        }
    }

    /// The end of a pane whose agent stopped on a 529 past its own retries.
    const OVERLOADED: &str = "❯ fix it\n\n● Half of it is done.\n\n● API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n────────\n❯ \n────────\n";

    /// The agent works again, then stops on `screen`.
    async fn turn_ends_on(fake: &FakeHerdr, pane: &str, screen: &str) {
        fake.set_status(pane, AgentStatus::Working);
        tokio::time::sleep(Duration::from_millis(50)).await;
        fake.set_pane_text(pane, screen);
        fake.set_status(pane, AgentStatus::Idle);
    }

    /// The next `task.rate_limited` about task `id`.
    async fn next_rate_limited(
        events: &mut broadcast::Receiver<PastorEvent>,
        id: i64,
    ) -> serde_json::Value {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("a task.rate_limited")
                .unwrap();
            if ev.kind == "task.rate_limited" && ev.task_id == Some(id) {
                return ev.detail.unwrap();
            }
        }
    }

    /// What pastor typed into `pane`: the retry prompt, then Enter.
    fn retries_sent(fake: &FakeHerdr, pane: &str) -> usize {
        fake.pane_input(pane)
            .iter()
            .filter(|i| {
                matches!(i, crate::herdr::fake::PaneInput::Text(t) if t == crate::task::RATE_RETRY_PROMPT)
            })
            .count()
    }

    /// A 529 past Claude's own retries: the task keeps its pane, its slot
    /// and its state, and after each wait of `rate_backoff` gets
    /// `RATE_RETRY_PROMPT` in its pane, with `task.rate_limited` (`line`,
    /// `attempt`, `retry_at`) before each. After `rate_retries` of them it
    /// is handled as a hard limit with no reset: it waits.
    #[tokio::test(start_paused = true)]
    async fn a_short_limit_is_retried_in_the_pane_then_handled_as_a_limit() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, short_limits(3));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        for (attempt, wait) in [(1, 1), (2, 2), (3, 3)] {
            turn_ends_on(&fake, &pane, OVERLOADED).await;
            let before = Utc::now();
            let detail = next_rate_limited(&mut events, t.id).await;
            assert_eq!(detail["attempt"], attempt, "{detail}");
            assert!(
                detail["line"]
                    .as_str()
                    .unwrap()
                    .starts_with("API Error: 529"),
                "{detail}"
            );
            let retry_at: chrono::DateTime<Utc> =
                serde_json::from_value(detail["retry_at"].clone()).unwrap();
            let secs = (retry_at - before).num_milliseconds();
            assert!(
                (wait * 1000 - 500..=wait * 1000 + 500).contains(&secs),
                "{secs}ms"
            );
            let row = store.get_task(t.id).unwrap().unwrap();
            assert_eq!(row.state, TaskState::Running, "no state change");
            assert_eq!(row.pane_id.as_deref(), Some(pane.as_str()));
            assert_eq!(
                retries_sent(&fake, &pane),
                attempt as usize - 1,
                "not before the wait"
            );
            tokio::time::sleep(Duration::from_millis(wait as u64 * 1000 + 300)).await;
            assert_eq!(
                retries_sent(&fake, &pane),
                attempt as usize,
                "after the wait"
            );
            assert_eq!(state_of(&store, t.id), TaskState::Running);
        }
        turn_ends_on(&fake, &pane, OVERLOADED).await;
        wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
        let limits = store.limits().unwrap();
        assert_eq!(limits.len(), 1, "{limits:?}");
        assert!(!limits[0].hard);
        assert_eq!(limits[0].until, None);
        assert_eq!(retries_sent(&fake, &pane), 3);
    }

    /// The rate retry line goes through a Claude session's inbox socket,
    /// each attempt after the first saying which it is so Claude never drops
    /// it as the same text from the same sender. The task stays running at
    /// the idle `retry_later` took as its baseline until it works again.
    #[tokio::test(start_paused = true)]
    async fn a_rate_retry_goes_through_the_inbox_socket() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, short_limits(3));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let retries = |fake: &FakeHerdr| -> Vec<String> {
            fake.inbox_sends()
                .into_iter()
                .map(|s| s.text)
                .filter(|t| t.starts_with(crate::task::RATE_RETRY_PROMPT))
                .collect()
        };
        for (attempt, wait) in [(1, 1), (2, 2)] {
            turn_ends_on(&fake, &pane, OVERLOADED).await;
            next_rate_limited(&mut events, t.id).await;
            tokio::time::sleep(Duration::from_millis(wait * 1000 + 300)).await;
            assert_eq!(retries(&fake).len(), attempt, "after the wait");
            assert_eq!(state_of(&store, t.id), TaskState::Running);
        }
        let sent = retries(&fake);
        assert_eq!(sent[0], crate::task::RATE_RETRY_PROMPT);
        assert!(sent[1].contains("Attempt 2"), "{sent:?}");
        assert_eq!(retries_sent(&fake, &pane), 0, "nothing typed");
    }

    /// A rate retry the socket does not take is typed.
    #[tokio::test(start_paused = true)]
    async fn a_rate_retry_the_socket_does_not_take_is_typed() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, short_limits(3));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_inbox_result(Some("held"));
        turn_ends_on(&fake, &pane, OVERLOADED).await;
        next_rate_limited(&mut events, t.id).await;
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert_eq!(retries_sent(&fake, &pane), 1);
        assert_eq!(fake.inbox_sends().len(), 2, "the socket was tried");
    }

    /// A message that names a wait (`try again in 20s`) is waited at least
    /// that long, past a shorter `rate_backoff`.
    #[tokio::test(start_paused = true)]
    async fn a_short_limit_waits_at_least_the_time_it_names() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, short_limits(3));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        turn_ends_on(
            &fake,
            &pane,
            "❯ fix it\n\n● API Error: 429 rate_limit_error, try again in 20s\n\n────────\n❯ \n────────\n",
        )
        .await;
        let before = Utc::now();
        let detail = next_rate_limited(&mut events, t.id).await;
        let retry_at: chrono::DateTime<Utc> =
            serde_json::from_value(detail["retry_at"].clone()).unwrap();
        assert!((retry_at - before).num_seconds() >= 19, "{detail}");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(retries_sent(&fake, &pane), 0, "the message's 20s, not 1s");
        tokio::time::sleep(Duration::from_secs(16)).await;
        assert_eq!(retries_sent(&fake, &pane), 1);
    }

    /// A retry that works (the turn ends on anything but a short limit)
    /// starts the count over: with one retry allowed, the next 529 of a
    /// task sent more work is retried again, not a limit.
    #[tokio::test(start_paused = true)]
    async fn a_retry_that_works_starts_the_count_over() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, short_limits(1));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        turn_ends_on(&fake, &pane, OVERLOADED).await;
        assert_eq!(next_rate_limited(&mut events, t.id).await["attempt"], 1);
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert_eq!(retries_sent(&fake, &pane), 1);
        turn_ends_on(
            &fake,
            &pane,
            "❯ fix it\n\n● All done.\n\n────────\n❯ \n────────\n",
        )
        .await;
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        h.send(
            t.id,
            SendInput {
                text: Some("one more thing".into()),
                enter: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        turn_ends_on(&fake, &pane, OVERLOADED).await;
        assert_eq!(next_rate_limited(&mut events, t.id).await["attempt"], 1);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(store.limits().unwrap().is_empty());
    }

    /// A screen of `tests/fixtures/pickers/`, without its header.
    fn picker_screen(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pickers")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with("#! "))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    /// A task whose agent goes `blocked` on `screen`, on a head that keeps
    /// the limits: the fake, the store, the task and its pane.
    async fn blocked_on(
        screen: &str,
    ) -> (
        FakeHerdr,
        Arc<Store>,
        Task,
        String,
        broadcast::Receiver<PastorEvent>,
    ) {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, events) =
            spawn_with_settings(&fake, &store, limits_kept(Duration::from_millis(100)));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(&pane, screen);
        fake.set_status(&pane, AgentStatus::Blocked);
        // The handle has to outlive the test's waits.
        std::mem::forget(h);
        (fake, store, t, pane, events)
    }

    /// The keys pressed in `pane`, all of them.
    fn keys_in(fake: &FakeHerdr, pane: &str) -> Vec<String> {
        fake.pane_input(pane)
            .into_iter()
            .flat_map(|i| match i {
                crate::herdr::fake::PaneInput::Keys(k) => k,
                crate::herdr::fake::PaneInput::Text(t) => vec![format!("text:{t}")],
            })
            .collect()
    }

    /// Claude's limit picker: pastor picks "Stop and wait for limit to
    /// reset" by its text, wherever it is in the list, and the task waits
    /// for its reset as one that stopped on the limit does. Nothing else is
    /// ever pressed: no key reaches extra usage or an upgrade.
    #[tokio::test(start_paused = true)]
    async fn a_blocked_agent_at_the_limit_picker_stops_and_waits() {
        for (screen, keys) in [
            ("claude-picker.txt", vec!["Enter"]),
            ("claude-picker-reordered.txt", vec!["Down", "Down", "Enter"]),
        ] {
            let (fake, store, t, pane, mut events) = blocked_on(&picker_screen(screen)).await;
            wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
            let mut want: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            want.push("esc".into());
            assert_eq!(keys_in(&fake, &pane), want, "{screen}");
            let row = store.get_task(t.id).unwrap().unwrap();
            assert_eq!(row.pane_id, None, "{screen}");
            assert!(row.waiting_until.is_some(), "{screen}");
            let limits = store.limits().unwrap();
            assert_eq!(limits.len(), 1, "{screen}: {limits:?}");
            assert!(limits[0].hard, "{screen}");
            assert!(saw(&mut events, "task.waiting", t.id), "{screen}");
        }
    }

    /// A limit picker with no "Stop and wait" (or in words pastor does not
    /// know) gets no key at all: the task stays `blocked`, its error saying
    /// what it looks like, for a person to answer. The pane is read once
    /// per blocked spell.
    #[tokio::test(start_paused = true)]
    async fn a_limit_picker_without_stop_and_wait_gets_no_key() {
        for screen in [
            "claude-picker-no-stop.txt",
            "claude-picker-unknown-wording.txt",
        ] {
            let (fake, store, t, pane, _events) = blocked_on(&picker_screen(screen)).await;
            wait_for("error", || {
                store
                    .get_task(t.id)
                    .unwrap()
                    .unwrap()
                    .error
                    .is_some_and(|e| e.contains("limit picker"))
            })
            .await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            let row = store.get_task(t.id).unwrap().unwrap();
            assert_eq!(row.state, TaskState::Blocked, "{screen}");
            assert!(keys_in(&fake, &pane).is_empty(), "{screen}");
            assert!(store.limits().unwrap().is_empty(), "{screen}");
            // One read at its start, for its login screen, one for a
            // startup dialog its prompt may have gone into
            // (`prompt_in_dialog`), and one for the picker.
            let reads = calls(&fake, "agent.read").len();
            assert_eq!(reads, 3, "{screen}: read once");
        }
    }

    /// Blocked on anything else (a permission prompt), the task is left
    /// for a person as before: nothing pressed, no error written.
    #[tokio::test(start_paused = true)]
    async fn a_blocked_agent_on_another_dialog_is_left_alone() {
        let (fake, store, t, pane, _events) =
            blocked_on(&picker_screen("no-picker-permission-prompt.txt")).await;
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Blocked);
        assert_eq!(row.error, None);
        assert!(keys_in(&fake, &pane).is_empty());
    }

    /// A limit further up the pane than the last prompt is an old one:
    /// the task is done. So is one whose agent said it was (`task done`),
    /// whatever its pane shows.
    #[tokio::test(start_paused = true)]
    async fn a_limit_above_the_last_prompt_or_after_task_done_is_no_limit() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, _events) = spawn_with_settings(&fake, &store, limits_kept(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(
            &pane,
            "● You've hit your limit · resets 3am\n\n❯ go on\n\n● Finished it.\n\n────────\n❯ \n────────\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(store.limits().unwrap().is_empty());

        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        h.end(t.id, None).await.unwrap();
        fake.set_pane_text(
            &pane,
            "● You've hit your limit · resets 3am\n\n────────\n❯ \n────────\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 4).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Done);
        assert!(row.ended);
        assert!(store.limits().unwrap().is_empty());
    }

    /// An agent that dies at its start on a usage limit: the task waits
    /// for the reset instead of failing, and drops the session it never
    /// got its prompt in, to start again from its prompt.
    #[tokio::test(start_paused = true)]
    async fn an_agent_that_dies_at_start_on_a_limit_waits_not_fails() {
        let fake = FakeHerdr::new();
        fake.exit_agents_on_start(true);
        fake.set_exit_screen("You've hit your limit · resets 3am\n");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) =
            spawn_with_settings(&fake, &store, limits_kept(Duration::from_millis(100)));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Waiting);
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Waiting);
        assert_eq!(row.spec.session_id, None);
        assert_eq!(row.pane_id, None);
        assert!(row.waiting_until.is_some());
        assert_eq!(store.limits().unwrap().len(), 1);
        assert!(saw(&mut events, "task.waiting", t.id));

        // Without a table (a headless serve) it fails as before.
        let fake = FakeHerdr::new();
        fake.exit_agents_on_start(true);
        fake.set_exit_screen("You've hit your limit · resets 3am\n");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        assert!(h.dispatch(t.id).await.is_err());
        assert_eq!(state_of(&store, t.id), TaskState::Failed);
    }

    /// `new_task` with its agent `agent`.
    fn task_of(store: &Store, agent: &str) -> Task {
        let mut spec = spec();
        spec.agent = agent.into();
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec,
                flock: "default".into(),
            })
            .unwrap()
    }

    /// The kinds of the events about task `id` received so far, with their
    /// details.
    fn kinds_of(
        events: &mut broadcast::Receiver<PastorEvent>,
        id: i64,
    ) -> Vec<(String, Option<serde_json::Value>)> {
        let mut kinds = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if ev.task_id == Some(id) {
                kinds.push((ev.kind, ev.detail));
            }
        }
        kinds
    }

    /// An agent that starts at its login screen is signed out: the task
    /// waits, held, not failed, with the agent marked signed out
    /// (`agent.signed_out`) and nothing typed into the screen. Claude's
    /// screen reads `blocked`; agy's reads idle and ready.
    #[tokio::test(start_paused = true)]
    async fn an_agent_at_its_login_screen_at_start_is_signed_out() {
        let screens = [
            (
                "claude",
                false,
                "Welcome to Claude Code\n\n Select login method:\n\n \u{276f} 1. Claude account with subscription\n   2. Anthropic Console account\n",
            ),
            (
                "agy",
                true,
                "Welcome back! You are currently not signed in.\n\n> Sign in with Google\n",
            ),
        ];
        for (agent, idle, screen) in screens {
            let fake = FakeHerdr::new();
            fake.set_trust_prompt(Some(vec!["Enter".into()]));
            fake.set_trust_idle(idle);
            fake.set_trust_screen(screen);
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, mut events) =
                spawn_with_settings(&fake, &store, limits_kept(Duration::from_millis(100)));
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            let t = h.dispatch(task_of(&store, agent).id).await.unwrap();
            assert_eq!(t.state, TaskState::Waiting, "{agent}: {:?}", t.error);
            let row = store.get_task(t.id).unwrap().unwrap();
            assert_eq!(row.pane_id, None, "{agent}");
            assert_eq!(row.waiting_until, Some(crate::limit::signed_out_until()));
            assert_eq!(
                row.error.as_deref(),
                Some(format!("m/{agent} signed out (seen by t-{})", t.id).as_str())
            );
            let limits = store.limits().unwrap();
            assert_eq!(limits.len(), 1, "{agent}: {limits:?}");
            assert!(limits[0].signed_out, "{agent}");
            assert_eq!(limits[0].account, format!("m/{agent}"));
            assert!(calls(&fake, "agent.prompt").is_empty(), "{agent}");
            let kinds = kinds_of(&mut events, t.id);
            let (_, detail) = kinds
                .iter()
                .find(|(k, _)| k == "agent.signed_out")
                .unwrap_or_else(|| panic!("{agent}: {kinds:?}"));
            assert_eq!(detail.as_ref().unwrap()["agent"], agent);
            assert!(!kinds.iter().any(|(k, _)| k == "agent.exhausted"));
            assert!(!kinds.iter().any(|(k, _)| k == "task.failed"));
            let (_, waiting) = kinds.iter().find(|(k, _)| k == "task.waiting").unwrap();
            assert_eq!(
                waiting.as_ref().unwrap()["shown"],
                format!("waiting: m/{agent} signed out")
            );
        }
    }

    /// A running Claude task whose login goes while it works ends its turn
    /// on `Not logged in · Please run /login`: it is held, waiting, not
    /// done or failed, and the agent is marked signed out. The same words
    /// in a tool call's output are not a sign-out.
    #[tokio::test(start_paused = true)]
    async fn a_running_task_whose_login_goes_is_held() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, mut events) = spawn_with_settings(&fake, &store, limits_kept(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(
            &pane,
            "❯ hi\n\n● Half of it is done.\n\n  ⎿  Not logged in · Please run /login\n\n────────\n❯ \n────────\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.pane_id, None);
        assert_eq!(row.machine.as_deref(), Some("m"));
        let limits = store.limits().unwrap();
        assert_eq!(limits.len(), 1, "{limits:?}");
        assert!(limits[0].signed_out);
        assert_eq!(limits[0].line, "Not logged in · Please run /login");
        assert_eq!(
            row.spec.rounds.stop.as_ref().map(|s| s.why.as_str()),
            Some("signed_out")
        );
        let kinds = kinds_of(&mut events, t.id);
        let has = |k: &str| kinds.iter().any(|(kind, _)| kind == k);
        assert!(has("agent.signed_out") && has("task.waiting"), "{kinds:?}");
        assert!(!has("task.done") && !has("task.failed"), "{kinds:?}");

        // A grep that prints the message, then a reply: done, no mark.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fake = FakeHerdr::new();
        let (h, _events) = spawn_with_settings(&fake, &store, limits_kept(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(
            &pane,
            "❯ hi\n\n● Bash(grep -rn login src)\n  ⎿  Not logged in · Please run /login\n\n────────\n❯ \n────────\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(store.limits().unwrap().is_empty());
    }

    /// A running task whose agent stops at its login screen, which herdr
    /// reads `blocked`, is held as signed out, not left blocked.
    #[tokio::test(start_paused = true)]
    async fn a_blocked_agent_at_its_login_screen_is_held() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, mut events) = spawn_with_settings(&fake, &store, limits_kept(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(
            &pane,
            "● Half of it is done.\n\n Select login method:\n\n \u{276f} 1. Claude account with subscription\n   2. Anthropic Console account\n",
        );
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("waiting", || state_of(&store, t.id) == TaskState::Waiting).await;
        assert!(store.limits().unwrap()[0].signed_out);
        assert!(saw(&mut events, "agent.signed_out", t.id));
    }

    /// Claude can end its turn with a command left running in the
    /// background; its footer says "1 shell still running" and it takes the
    /// turn up again when the shell ends. herdr reads it idle all the while,
    /// but the task is not done: it stays running through settle windows
    /// until the shell has ended and the agent went idle again.
    #[tokio::test(start_paused = true)]
    async fn an_agent_waiting_on_a_background_shell_is_not_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(
            &pane,
            "● Running make check in the background.\n\n❯\n  ⏵⏵ auto mode on · 1 shell still running\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 6).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "a background shell keeps the task running"
        );

        // The shell ended: the agent takes the turn up, pushes, goes idle.
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● make check passed. Pushed the branch.\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// Claude 2.1.3xx draws the shell as a bare count in its footer
    /// (`· 1 shell ·`), with no "still running": the task stays running
    /// all the same, and is done once the shell ended and the agent's next
    /// turn did.
    #[tokio::test(start_paused = true)]
    async fn a_bare_shell_count_in_the_footer_is_not_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        let screen: String = include_str!("../tests/fixtures/panes/one-shell-idle-footer.txt")
            .lines()
            .filter(|l| !l.starts_with("#! "))
            .map(|l| format!("{l}\n"))
            .collect();
        fake.set_pane_text(&pane, &screen);
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 6).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "a background shell keeps the task running"
        );
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● make check passed. Committed and pushed.\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// The same with a dialog Claude drew under its turn summary, in place
    /// of the input box: the shell is still running, so the task is not
    /// closed as finished with no summary while the agent waits on it.
    #[tokio::test(start_paused = true)]
    async fn an_agent_waiting_on_a_shell_under_a_dialog_is_not_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        let screen: String =
            include_str!("../tests/fixtures/panes/shell-running-under-a-dialog.txt")
                .lines()
                .filter(|l| !l.starts_with("#! "))
                .map(|l| format!("{l}\n"))
                .collect();
        fake.set_pane_text(&pane, &screen);
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 6).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "a background shell keeps the task running"
        );
    }

    /// A task of `agent` (no `[agents]` entry, so its kind is its name).
    fn new_task_of(store: &Store, agent: &str) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    agent: agent.into(),
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    /// agy idles in a long thinking pause, or while a command it started
    /// runs, and herdr reads that as it reads the end of its turn. So a task
    /// whose agent is not Claude is not done on an idle: it goes `blocked`,
    /// "idle without task done" and the pane's last lines, its pane is not
    /// closed, it runs again when the agent does, and it is done when the
    /// agent says so.
    #[tokio::test(start_paused = true)]
    async fn a_non_claude_task_idle_without_task_done_is_blocked() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, mut events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task_of(&store, "agy");
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(
            &pane,
            "  Reading the diff.\n\n  Thinking about the lock order\n\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            row.error.as_deref(),
            Some("idle without task done: Reading the diff. / Thinking about the lock order")
        );
        assert_eq!(row.finished_at, None);
        let ev = loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            if ev.kind != "task.running" {
                break ev;
            }
        };
        assert_eq!(ev.kind, "task.blocked");
        assert_eq!(
            ev.detail,
            Some(serde_json::json!({
                "why": "idle_without_task_done",
                "tail": "Reading the diff.\nThinking about the lock order",
            }))
        );
        tokio::time::sleep(settle * 6).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(fake.agents().iter().any(|a| a.pane_id == pane), "pane kept");

        // The pause is over: it works again, and ends with `task done`.
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        assert_eq!(store.get_task(t.id).unwrap().unwrap().error, None);
        h.end(t.id, Some("done: reviewed".into())).await.unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 6).await;
        assert_eq!(state_of(&store, t.id), TaskState::Done);
    }

    /// agy stops on a permission prompt idle too; that is a question, and
    /// its footer's `· 1 task` (a command of its own still going) keeps the
    /// task running until the agent takes its turn up again.
    #[tokio::test(start_paused = true)]
    async fn agy_permission_prompts_block_and_its_tasks_footer_keeps_running() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task_of(&store, "agy");
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(
            &pane,
            "  Started make mutants.\n\n  > \n ~/src/repo · 1 task\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 6).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "a task of its own keeps the task running"
        );

        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(
            &pane,
            "  $ make deploy\n  Run this command? (y/n)\n\n  > \n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().error.as_deref(),
            Some("agent asked: Run this command? (y/n)")
        );
    }

    /// A task whose prompt never asked for `task done` (`summary = "off"`)
    /// ends on the idle, as before; so does a Claude task, whatever its
    /// pane says.
    #[tokio::test(start_paused = true)]
    async fn a_task_never_asked_for_task_done_is_done_on_the_idle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    agent: "codex".into(),
                    summary: crate::task::SummaryMode::Off,
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap();
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(&pane, "  Pushed the branch.\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;

        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(&pane, "  Run this command?\n ~/src · 1 task\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// The pane text read to settle a task as done is kept as its tail, for
    /// its connector's finish command (`Store::take_pane_tail`).
    #[tokio::test(start_paused = true)]
    async fn a_done_task_leaves_the_end_of_its_pane_for_the_finish_command() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(100)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● Opened https://example.org/pr/7\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        let tail = store.take_pane_tail(t.id).expect("a tail was kept");
        assert!(tail.contains("Opened https://example.org/pr/7"), "{tail}");
    }

    /// A pane read that never answers is an outage, not an empty pane: the
    /// task must not be settled `done` on it. The actor reconnects, the task
    /// stays pending, and the next settle check reads the question.
    #[tokio::test(start_paused = true)]
    async fn a_pane_read_that_hangs_does_not_settle_a_question_as_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(200);
        let settings = MachineSettings {
            request_timeout: Duration::from_millis(300),
            ..settings_with_settle(settle)
        };
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● Should I keep the old flag?\n\n❯\n");
        fake.hang_method("agent.read");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
    }

    /// herdr rejects a prompt to a blocked agent instead of queueing it, so
    /// dispatch must not send one: it leaves the task `blocked` with the
    /// prompt pending. Flipping the task to `running` on its own would leave
    /// the agent with nothing to do, so the prompt has to be sent once a
    /// human clears the block.
    #[tokio::test(start_paused = true)]
    async fn a_blocked_agent_is_not_prompted_at_dispatch_but_is_once_it_clears() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(150));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(store.get_task(t.id).unwrap().unwrap().prompt_pending);
        let prompts = || {
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.prompt")
                .count()
        };
        assert_eq!(prompts(), 0, "a blocked agent is not prompted at dispatch");

        // The human answers the agent's startup question; it goes idle.
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(
            prompts(),
            1,
            "the prompt is sent exactly once, after the block clears"
        );
        let t = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(t.error, None, "the blocked advice is gone");
        assert_eq!(
            fake.agents()[0].agent_status,
            AgentStatus::Working,
            "the agent took the prompt"
        );
    }

    /// The folder trust dialog case end to end: the agent is blocked before it
    /// finishes launching, dispatch leaves the task `blocked` with the prompt
    /// pending and never calls `agent.prompt`, and once a human answers,
    /// herdr may still refuse with `agent_not_ready` for a moment (it is
    /// still within its launching window). Reconcile keeps retrying until the
    /// prompt lands, then the task runs.
    ///
    /// `ready_after` is well above `READY_POLL` (`dispatch.rs`) and above
    /// `agent_ready_timeout` in `settings()`: whichever `agent.list` poll
    /// dispatch's readiness wait makes, the agent is still inside its
    /// launching window, so `agent.list` reports it `blocked` with
    /// `launch_pending: true` every time. That is what forces dispatch
    /// through the blocked-while-launching path deterministically, instead
    /// of racing to see whether the launching window happens to have closed
    /// by the time dispatch polls again.
    #[tokio::test(start_paused = true)]
    async fn an_agent_blocked_at_launch_gets_its_prompt_once_answered() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_secs(2));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.prompt_pending);
        let prompts = || {
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.prompt")
                .count()
        };
        assert_eq!(
            prompts(),
            0,
            "a blocked-while-launching agent is not prompted at dispatch"
        );

        // Answered while the fake still counts the agent as launching: the
        // first delivery attempt is refused and has to be retried. `seq`
        // increments on `set_status` and on an *accepted* `agent.prompt`
        // (fake.rs only bumps it past the checks), never on a refusal, so the
        // delta below counts accepted state changes, not raw request counts.
        let seq_before_clear = fake.agents()[0].state_change_seq;
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(fake.agents()[0].agent_status, AgentStatus::Working);
        assert_eq!(
            fake.agents()[0].state_change_seq,
            seq_before_clear + 2,
            "exactly two accepted changes: the block clearing to idle, then \
             one accepted prompt; every agent_not_ready retry in between left \
             the sequence untouched"
        );
        let sent = fake
            .requests()
            .into_iter()
            .rfind(|r| r.method == "agent.prompt")
            .expect("at least one agent.prompt request was made");
        assert_eq!(
            sent.params["text"],
            format!("hi\n\n{}", crate::task::SUMMARY_ASK),
            "our prompt, not a stray one, asking for a summary"
        );
    }

    /// A startup block cleared past the old deadline gets a fresh timeout
    /// too: `deliver_pending_prompt` takes the same branch `apply` does for
    /// a later block, not the time spent waiting for a person at launch.
    #[tokio::test(start_paused = true)]
    async fn a_startup_block_cleared_past_its_timeout_gets_fresh_time() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_secs(2));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task_timing_out(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.prompt_pending);
        start_an_hour_ago(&store, t.id);
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        // Some reconciles later, within the new timeout, not the old one.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// A store error in reconcile is pastor's problem, not the machine's: the
    /// channel stays connected and no `machine.lost` goes out.
    #[tokio::test(start_paused = true)]
    async fn a_local_error_in_reconcile_is_not_an_outage() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        store.execute_raw(&format!(
            "UPDATE tasks SET machine = 'm', state = 'running', created_at = 'not a date' WHERE id = {}",
            t.id
        ));
        // Several reconcile ticks (200ms each) run into the corrupt row.
        let deadline = Instant::now() + Duration::from_millis(800);
        while let Ok(ev) = tokio::time::timeout_at(deadline, events.recv()).await {
            assert_ne!(ev.unwrap().kind, "machine.lost");
        }
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
    }

    /// Recovering from `Blocked` must not carry the startup advice onto a task
    /// that is running again, and a `Done` task that goes back to work must not
    /// keep the finish time of the cycle it just left.
    #[tokio::test(start_paused = true)]
    async fn state_changes_clear_the_previous_state_s_metadata() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(100)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();

        // A task that blocks carries dispatch's advice, then answers the prompt.
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let mut blocked = store.get_task(t.id).unwrap().unwrap();
        blocked.error = Some("agent blocked during startup; answer its prompt".into());
        store.update_task(&mut blocked).unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running again", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().error,
            None,
            "the blocked advice outlived the blocked state"
        );

        // A done task that goes back to work loses the old finish time.
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(store.get_task(t.id).unwrap().unwrap().finished_at.is_some());
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("working again", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().finished_at,
            None,
            "a running task kept the finish time of its last cycle"
        );
    }

    /// The pane of a finished task closing later is not when its work
    /// finished: `done -> closed` keeps `finished_at`.
    #[tokio::test(start_paused = true)]
    async fn closing_a_done_task_keeps_its_finish_time() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        let finished = store.get_task(t.id).unwrap().unwrap().finished_at;
        assert!(finished.is_some());
        tokio::time::sleep(Duration::from_millis(20)).await;
        fake.close_pane(&pane);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        assert_eq!(store.get_task(t.id).unwrap().unwrap().finished_at, finished);
    }

    /// A pane adopted by a reconcile while connected is not in the event
    /// subscription that is already open; the actor must resubscribe so its
    /// status changes arrive as events, not only at the next reconcile.
    #[tokio::test(start_paused = true)]
    async fn a_pane_adopted_while_connected_is_subscribed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake
            .workspace_create(None, &name, &Default::default())
            .await
            .unwrap();
        let pane = created.root_pane.pane_id.clone();
        fake.agent_start(&name, "claude", &pane, &[]).await.unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        store.update_task(&mut t).unwrap();
        wait_for("adopted", || {
            store.get_task(t.id).unwrap().unwrap().pane_id.as_deref() == Some(pane.as_str())
        })
        .await;
        wait_for("a subscription covering the pane", || {
            fake.requests().iter().any(|r| {
                r.method == "events.subscribe"
                    && r.params["subscriptions"]
                        .as_array()
                        .is_some_and(|subs| subs.iter().any(|s| s["pane_id"] == pane.as_str()))
            })
        })
        .await;
    }

    /// A pane that closes between the reconcile that adopted it and the
    /// resubscribe gets that subscription refused with herdr's
    /// `pane_not_found`, an API error: the machine answers, so it polls and
    /// subscribes again instead of announcing `machine.lost`.
    #[tokio::test(start_paused = true)]
    async fn a_pane_gone_before_the_resubscribe_is_not_an_outage() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let pane = start_agent(&fake, &name).await;
        fake.set_status(&pane, AgentStatus::Working);
        fake.close_pane_before_subscribe(&pane);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        store.update_task(&mut t).unwrap();
        let names_pane = |r: &crate::herdr::Request| {
            r.method == "events.subscribe"
                && r.params["subscriptions"]
                    .as_array()
                    .is_some_and(|subs| subs.iter().any(|s| s["pane_id"] == pane.as_str()))
        };
        wait_for("the refused resubscribe", || {
            fake.requests().iter().any(&names_pane)
        })
        .await;
        // Polling reconciles the closed pane away and subscribes again.
        wait_for("a later subscribe", || {
            let reqs = fake.requests();
            let refused = reqs.iter().position(&names_pane).unwrap();
            reqs[refused + 1..]
                .iter()
                .any(|r| r.method == "events.subscribe")
        })
        .await;
        wait_for("connected again", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        while let Ok(ev) = events.try_recv() {
            assert_ne!(ev.kind, "machine.lost", "{ev:?}");
        }
    }

    /// Start an agent named `name` in a fresh workspace; returns its pane.
    async fn start_agent(fake: &FakeHerdr, name: &str) -> String {
        let created = fake
            .workspace_create(None, name, &Default::default())
            .await
            .unwrap();
        fake.agent_start(name, "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        created.root_pane.pane_id
    }

    #[test]
    fn only_task_shaped_names_are_task_agents() {
        assert_eq!(task_id_of_agent("t-12"), Some(12));
        for name in ["12", "t-", "t-1a", "t-+1", "x-1", "t-1 "] {
            assert_eq!(task_id_of_agent(name), None, "{name:?}");
        }
    }

    /// `task close t-01` parses id 1 and looks for `t-1`, so an agent named
    /// `t-01` reported as an orphan could never be closed. Only the names
    /// `Task::agent_name_for` makes count.
    #[test]
    fn only_canonical_task_names_are_orphans() {
        for name in ["t-01", "t-07", "t-0", "t-00"] {
            assert_eq!(task_id_of_agent(name), None, "{name:?}");
        }
        let agent = |name: &str, pane: &str| -> AgentInfo {
            serde_json::from_value(serde_json::json!({
                "pane_id": pane, "workspace_id": "w", "tab_id": "tab",
                "name": name, "agent_status": "idle",
            }))
            .unwrap()
        };
        let agents = [agent("t-01", "p1"), agent("t-0", "p2"), agent("t-3", "p3")];
        assert_eq!(
            orphan_agents(&agents, &[]),
            vec![("t-3".to_string(), "p3".to_string())]
        );
    }

    /// An agent named like a task that no open task owns is an orphan: its
    /// row failed, closed or is gone. It holds a pane, so it counts in `live`,
    /// and it is named in the status; with `close_failed_after`
    /// off, nothing closes it.
    #[tokio::test(start_paused = true)]
    async fn reconcile_reports_orphaned_agents() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut failed = new_task(&store);
        let mut running = new_task(&store);
        let failed_pane = start_agent(&fake, &Task::agent_name_for(failed.id)).await;
        let running_pane = start_agent(&fake, &Task::agent_name_for(running.id)).await;
        let ghost_pane = start_agent(&fake, "t-99").await;
        start_agent(&fake, "mine").await;
        start_agent(&fake, "7").await;
        failed.state = TaskState::Failed;
        failed.machine = Some("m".into());
        failed.pane_id = Some(failed_pane.clone());
        store.update_task(&mut failed).unwrap();
        running.state = TaskState::Running;
        running.machine = Some("m".into());
        running.pane_id = Some(running_pane);
        running.agent_name = Some(Task::agent_name_for(running.id));
        store.update_task(&mut running).unwrap();

        let (h, _events) = spawn(&fake, &store);
        wait_for("orphans", || !h.snapshot().orphans.is_empty()).await;
        let mut orphans = h.snapshot().orphans;
        orphans.sort();
        assert_eq!(
            orphans,
            vec![Task::agent_name_for(failed.id), "t-99".into()]
        );
        assert_eq!(h.snapshot().live, 3, "one running task plus two orphans");
        assert_eq!(state_of(&store, failed.id), TaskState::Failed, "left alone");
        assert!(
            fake.agents().iter().any(|a| a.pane_id == ghost_pane),
            "never closed on its own"
        );

        fake.close_pane(&ghost_pane);
        wait_for("the closed orphan dropped", || {
            h.snapshot().orphans.len() == 1
        })
        .await;
        assert_eq!(h.snapshot().live, 2);
    }

    /// The case orphans exist for: dispatch failed after `agent.start`, so the
    /// task is failed while its agent is still in its pane.
    #[tokio::test(start_paused = true)]
    async fn a_dispatch_that_fails_after_agent_start_leaves_an_orphan() {
        let fake = FakeHerdr::new();
        fake.exit_agents_listed(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        h.dispatch(t.id).await.unwrap_err();
        assert_eq!(state_of(&store, t.id), TaskState::Failed);
        wait_for("orphan", || {
            h.snapshot().orphans == vec![Task::agent_name_for(t.id)]
        })
        .await;
        assert_eq!(h.snapshot().live, 1);
    }

    /// herdr's agy hook reports the conversation id through `agent_session`
    /// once it has one; the next reconcile that lists it records it on the
    /// task, the same place Claude's own `--session-id` goes, so a later
    /// pause or `task attach` can resume it on `agy --conversation <id>`.
    #[tokio::test(start_paused = true)]
    async fn an_agy_conversation_id_herdr_reports_is_recorded_on_the_task() {
        use crate::herdr::{AgentSessionInfo, AgentSessionRefKind};

        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let mut t = new_task(&store);
        t.spec.agent = "agy".into();
        store.update_task(&mut t).unwrap();
        let running = h.dispatch(t.id).await.unwrap();
        assert_eq!(running.spec.session_id, None, "agy picks none at start");
        let pane = running.pane_id.clone().unwrap();
        const CONVERSATION: &str = "9f1e2d3c-4b5a-6978-8f90-123456789abc";
        fake.set_agent_session(
            &pane,
            Some(AgentSessionInfo {
                agent: "agy".into(),
                kind: AgentSessionRefKind::Id,
                source: "agy-hook".into(),
                value: CONVERSATION.into(),
            }),
        );
        wait_for("its conversation id is recorded", || {
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .spec
                .session_id
                .as_deref()
                == Some(CONVERSATION)
        })
        .await;
    }

    /// A machine with no agy hook installed (or one that has not reported
    /// anything yet) never sets `agent_session`, so the task simply keeps no
    /// session, as before: a plain `agent.list` report of a kind pastor
    /// does not resume, or one that does not match the task's own kind, is
    /// never recorded either.
    #[tokio::test(start_paused = true)]
    async fn an_agy_task_with_no_reported_conversation_keeps_none() {
        use crate::herdr::{AgentSessionInfo, AgentSessionRefKind};

        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let mut t = new_task(&store);
        t.spec.agent = "agy".into();
        store.update_task(&mut t).unwrap();
        let running = h.dispatch(t.id).await.unwrap();
        let pane = running.pane_id.clone().unwrap();
        // A report for another agent kind is never trusted for this task.
        fake.set_agent_session(
            &pane,
            Some(AgentSessionInfo {
                agent: "codex".into(),
                kind: AgentSessionRefKind::Id,
                source: "codex-hook".into(),
                value: "9f1e2d3c-4b5a-6978-8f90-123456789abc".into(),
            }),
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(store.get_task(t.id).unwrap().unwrap().spec.session_id, None);
    }

    /// An agy task's conversation id can be shaped like a Claude session,
    /// but its usage is never looked for in Claude's files.
    #[tokio::test(start_paused = true)]
    async fn an_agy_task_that_ends_reads_no_claude_usage() {
        use crate::herdr::{AgentSessionInfo, AgentSessionRefKind};

        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task_of(&store, "agy").id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        const CONVERSATION: &str = "9f1e2d3c-4b5a-4978-8f90-123456789abc";
        // What a Claude session of that id would have used, were it read.
        fake.set_usage(
            None,
            CONVERSATION,
            crate::usage::TaskUsage {
                model: None,
                api_calls: 1,
                input: 1,
                cache_write: 0,
                cache_read: 0,
                output: 1,
                read_at: Utc::now(),
            },
        );
        fake.set_agent_session(
            &pane,
            Some(AgentSessionInfo {
                agent: "agy".into(),
                kind: AgentSessionRefKind::Id,
                source: "agy-hook".into(),
                value: CONVERSATION.into(),
            }),
        );
        wait_for("its conversation id is recorded", || {
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .spec
                .session_id
                .as_deref()
                == Some(CONVERSATION)
        })
        .await;
        h.end(t.id, Some("done".into())).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(store.get_task(t.id).unwrap().unwrap().usage, None);
    }

    /// An agy task that recorded its conversation id can be paused and
    /// resumed exactly as a Claude one, on `--conversation` in place of
    /// `--resume`: `why_not_pausable` took only Claude before this.
    #[tokio::test(start_paused = true)]
    async fn a_paused_agy_task_resumes_its_own_conversation() {
        use crate::herdr::{AgentSessionInfo, AgentSessionRefKind};
        use crate::task::Priority;

        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = store
            .insert_task_at(
                NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "hi".into(),
                    spec: DispatchSpec {
                        agent: "agy".into(),
                        ..spec()
                    },
                    flock: "default".into(),
                },
                Priority::Low,
                None,
                crate::task::TaskRole::Agent,
            )
            .unwrap();
        let running = h.dispatch(t.id).await.unwrap();
        let pane = running.pane_id.clone().unwrap();
        const CONVERSATION: &str = "9f1e2d3c-4b5a-6978-8f90-123456789abc";
        fake.set_agent_session(
            &pane,
            Some(AgentSessionInfo {
                agent: "agy".into(),
                kind: AgentSessionRefKind::Id,
                source: "agy-hook".into(),
                value: CONVERSATION.into(),
            }),
        );
        wait_for("its conversation id is recorded", || {
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .spec
                .session_id
                .as_deref()
                == Some(CONVERSATION)
        })
        .await;

        let other = new_task(&store);
        let paused = h.pause(t.id, other.id).await.unwrap();
        assert_eq!(paused.state, TaskState::Paused);

        h.resume(t.id).await.unwrap();
        wait_for("resumed", || state_of(&store, t.id) == TaskState::Running).await;
        let reqs = fake.requests();
        let start = reqs
            .iter()
            .rev()
            .find(|r| r.method == "agent.start")
            .unwrap();
        assert_eq!(start.params["kind"], "agy");
        assert_eq!(
            start.params["args"],
            serde_json::json!(["--conversation", CONVERSATION])
        );
    }

    async fn connected(
        fake: &FakeHerdr,
        store: &Arc<Store>,
    ) -> (MachineHandle, broadcast::Receiver<PastorEvent>) {
        let (h, events) = spawn(fake, store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        (h, events)
    }

    fn calls(fake: &FakeHerdr, method: &str) -> Vec<serde_json::Value> {
        fake.requests()
            .into_iter()
            .filter(|r| r.method == method)
            .map(|r| r.params)
            .collect()
    }

    /// The `pane.close` calls that close a task's pane, without the one a
    /// worktree dispatch makes right after it splits the agent's pane off
    /// herdr's (see `dispatch`).
    fn closes(fake: &FakeHerdr) -> Vec<serde_json::Value> {
        let reqs = fake.requests();
        reqs.iter()
            .enumerate()
            .filter(|(i, r)| {
                r.method == "pane.close" && (*i == 0 || reqs[i - 1].method != "pane.split")
            })
            .map(|(_, r)| r.params.clone())
            .collect()
    }

    fn worktree_task(store: &Store) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    repo: Some("/r".into()),
                    worktree: true,
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    /// The fleet bug: `pastor task retry` of a failed worktree task died with
    /// git's "fatal: '<path>' already exists", because the failed task's
    /// checkout is still on disk. The retry works on in that checkout, on the
    /// same branch, instead of creating it again; a retry of the retry too.
    #[tokio::test(start_paused = true)]
    async fn a_retry_reopens_the_worktree_of_the_task_it_retries() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let first = h.dispatch(worktree_task(&store).id).await.unwrap();
        let branch = format!("pastor/t-{}", first.id);
        fake.exit_pane(first.pane_id.as_deref().unwrap());
        wait_for("failed", || state_of(&store, first.id) == TaskState::Failed).await;

        let first = store.get_task(first.id).unwrap().unwrap();
        let checkout = first.spec.checkout.clone().expect("checkout recorded");
        assert_eq!(checkout.branch, branch);
        let retry = store.insert_retry(first.id).unwrap();
        assert_eq!(retry.spec.reopen.as_ref().unwrap().path, checkout.path);
        let t = h.dispatch(retry.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
        assert_eq!(calls(&fake, "worktree.create").len(), 1, "created once");
        let opened = calls(&fake, "worktree.open");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0]["cwd"], "/r");
        assert_eq!(opened[0]["branch"], branch.as_str());

        fake.exit_pane(t.pane_id.as_deref().unwrap());
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        // A failed task ended its round too, with no summary.
        let failed = store.get_task(t.id).unwrap().unwrap().summary.unwrap();
        assert_eq!(failed.outcome, crate::task::Outcome::NoSummary);
        let again = store.insert_retry(t.id).unwrap();
        assert_eq!(again.spec.reopen.as_ref().unwrap().branch, branch);
        let t = h.dispatch(again.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
        assert_eq!(calls(&fake, "worktree.create").len(), 1);
        // The same checkout, reached through the workspace the first task
        // left open on it.
        let reached = t.spec.checkout.unwrap();
        assert_eq!(
            (reached.branch, reached.path),
            (checkout.branch, checkout.path)
        );
        assert!(reached.already_open);
    }

    /// A task can fail before herdr made its worktree, here because another
    /// task already has the branch the job names. That checkout is not the
    /// failed task's, so its retry must not reopen it: it gets a branch and a
    /// worktree of its own.
    #[tokio::test(start_paused = true)]
    async fn a_retry_never_reopens_a_checkout_its_task_did_not_make() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let on_branch = |store: &Store| {
            let mut t = worktree_task(store);
            t.spec.branch = Some("fix/x".into());
            store.update_task(&mut t).unwrap();
            t
        };
        let owner = h.dispatch(on_branch(&store).id).await.unwrap();
        assert_eq!(owner.state, TaskState::Running, "{:?}", owner.error);
        let loser = on_branch(&store);
        h.dispatch(loser.id).await.unwrap_err();
        let loser = store.get_task(loser.id).unwrap().unwrap();
        assert_eq!(loser.state, TaskState::Failed);
        assert_eq!(loser.spec.checkout, None);

        let retry = store.insert_retry(loser.id).unwrap();
        let t = h.dispatch(retry.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
        assert!(calls(&fake, "worktree.open").is_empty());
        let created = calls(&fake, "worktree.create");
        assert_eq!(created.len(), 3);
        assert_eq!(created[2]["branch"], format!("pastor/t-{}", retry.id));
    }

    /// A failed task whose agent is still listed may be at work in its
    /// checkout, and one whose checkout is now somewhere else is not the one
    /// it made. Neither is reopened: the retry gets a new branch and worktree.
    #[tokio::test(start_paused = true)]
    async fn a_retry_reopens_only_a_checkout_whose_agent_is_gone_at_the_same_path() {
        for moved in [false, true] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let first = h.dispatch(worktree_task(&store).id).await.unwrap();
            let mut first = store.get_task(first.id).unwrap().unwrap();
            first.state = TaskState::Failed;
            if moved {
                fake.exit_pane(first.pane_id.as_deref().unwrap());
                first.spec.checkout.as_mut().unwrap().path = "/elsewhere".into();
            }
            store.update_task(&mut first).unwrap();

            let retry = store.insert_retry(first.id).unwrap();
            let t = h.dispatch(retry.id).await.unwrap();
            assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
            assert!(calls(&fake, "worktree.open").is_empty(), "moved: {moved}");
            let created = calls(&fake, "worktree.create");
            assert_eq!(created.len(), 2);
            assert_eq!(created[1]["branch"], format!("pastor/t-{}", retry.id));
        }
    }

    /// Two retries of one failed task carry the same `reopen`. The first
    /// reopens the checkout and its agent is at work there under a new name,
    /// so the second must not reopen it too: any agent listed in the
    /// checkout's workspace keeps it, and the second gets a new branch and
    /// worktree.
    #[tokio::test(start_paused = true)]
    async fn a_second_retry_of_one_task_never_reopens_a_checkout_in_use() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let first = h.dispatch(worktree_task(&store).id).await.unwrap();
        fake.exit_pane(first.pane_id.as_deref().unwrap());
        wait_for("failed", || state_of(&store, first.id) == TaskState::Failed).await;

        let one = store.insert_retry(first.id).unwrap();
        let two = store.insert_retry(first.id).unwrap();
        let one = h.dispatch(one.id).await.unwrap();
        assert_eq!(one.state, TaskState::Running, "{:?}", one.error);
        assert_eq!(calls(&fake, "worktree.open").len(), 1);

        let two = h.dispatch(two.id).await.unwrap();
        assert_eq!(two.state, TaskState::Running, "{:?}", two.error);
        assert_eq!(calls(&fake, "worktree.open").len(), 1, "reopened once");
        let created = calls(&fake, "worktree.create");
        assert_eq!(created.len(), 2);
        assert_eq!(created[1]["branch"], format!("pastor/t-{}", two.id));
        assert_ne!(two.workspace_id, one.workspace_id);
    }

    /// A retry reopens its checkout through `worktree.open`, which answers a
    /// workspace already showing it with `already_open`: the failed task's
    /// own (its pane stays after the agent exits) or one someone opened
    /// since. That workspace is not the retry's, and its root pane is not
    /// pastor's to close; only the root of a workspace herdr just made goes.
    #[tokio::test(start_paused = true)]
    async fn a_retry_never_closes_a_pane_of_a_workspace_already_open() {
        for place in [Place::Own, Place::Pastor] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let first = h
                .dispatch(placed_task(&store, place.clone(), true).id)
                .await
                .unwrap();
            fake.exit_pane(first.pane_id.as_deref().unwrap());
            wait_for("failed", || state_of(&store, first.id) == TaskState::Failed).await;
            let first = store.get_task(first.id).unwrap().unwrap();
            let checkout = first.spec.checkout.clone().unwrap();
            // Someone's workspace on the checkout, with nobody at work in it.
            let ws = match place {
                Place::Own => first.workspace_id.clone().unwrap(),
                _ => {
                    fake.worktree_open("/r", &checkout.branch, "mine")
                        .await
                        .unwrap()
                        .workspace
                        .workspace_id
                }
            };
            let before = fake.panes(&ws);

            let opened = calls(&fake, "worktree.open").len();

            let retry = store.insert_retry(first.id).unwrap();
            let t = h.dispatch(retry.id).await.unwrap();
            assert_eq!(t.state, TaskState::Running, "{place:?}: {:?}", t.error);
            assert_eq!(calls(&fake, "worktree.open").len(), opened + 1, "{place:?}");
            let panes = fake.panes(&ws);
            for pane in &before {
                assert!(panes.contains(pane), "{place:?}: {pane} closed: {panes:?}");
            }
        }
    }

    /// A retry placed in a workspace of its own can still join one: its
    /// `worktree.open` answers the workspace already showing the checkout
    /// (`already_open`), the failed task's or someone's. The row records
    /// that, and removing the worktree would close that workspace with every
    /// pane in it, so the checkout stays with a note to remove it by hand,
    /// for `--remove-worktree` and auto-close alike; only the retry's own
    /// pane goes.
    #[tokio::test(start_paused = true)]
    async fn a_worktree_whose_workspace_was_already_open_is_never_removed() {
        for (place, auto) in [
            (Place::Own, false),
            (Place::Own, true),
            (Place::Repo, false),
            (Place::Repo, true),
        ] {
            let what = format!("{place:?}, auto {auto}");
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = spawn_with_settings(
                &fake,
                &store,
                if auto {
                    auto_close_settings()
                } else {
                    settings()
                },
            );
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            let first = h
                .dispatch(placed_task(&store, place.clone(), true).id)
                .await
                .unwrap();
            fake.exit_pane(first.pane_id.as_deref().unwrap());
            wait_for("failed", || state_of(&store, first.id) == TaskState::Failed).await;
            let ws = first.workspace_id.clone().unwrap();
            let before = fake.panes(&ws);

            let retry = store.insert_retry(first.id).unwrap();
            let t = h.dispatch(retry.id).await.unwrap();
            assert_eq!(t.state, TaskState::Running, "{what}: {:?}", t.error);
            assert_eq!(t.workspace_id.as_deref(), Some(ws.as_str()), "{what}");
            assert!(t.spec.checkout.as_ref().unwrap().already_open, "{what}");

            if auto {
                fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
                auto_closed(&store, t.id).await;
            } else {
                let closed = h.close(t.id, true).await.unwrap();
                assert_eq!(closed.state, TaskState::Closed, "{what}");
            }
            let row = store.get_task(t.id).unwrap().unwrap();
            let note = row.error.as_deref().unwrap_or_default();
            assert!(note.contains("worktree kept"), "{what}: {row:?}");
            assert!(note.contains("git worktree remove"), "{what}: {row:?}");
            assert!(calls(&fake, "worktree.remove").is_empty(), "{what}");
            assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1, "{what}");
            assert_eq!(fake.panes(&ws), before, "{what}");
        }
    }

    /// A failed worktree task placed in `pastor` leaves no workspace on its
    /// checkout, yet a fix round placed there too may still work in it from
    /// a pane of the shared workspace. A retry must not reopen a checkout
    /// someone works in: it looks for agents by the checkout's path, as
    /// removing it does, and gets a new branch and worktree instead.
    #[tokio::test(start_paused = true)]
    async fn a_retry_never_reopens_a_checkout_another_agent_works_in() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let first = h
            .dispatch(placed_task(&store, Place::Pastor, true).id)
            .await
            .unwrap();
        let checkout = first.spec.checkout.clone().unwrap();
        let mut fix = placed_task(&store, Place::Pastor, false);
        fix.spec.repo = Some(checkout.path.clone());
        store.update_task(&mut fix).unwrap();
        let fix = h.dispatch(fix.id).await.unwrap();
        assert_eq!(fix.state, TaskState::Running, "{:?}", fix.error);
        fake.exit_pane(first.pane_id.as_deref().unwrap());
        wait_for("failed", || state_of(&store, first.id) == TaskState::Failed).await;

        let retry = store.insert_retry(first.id).unwrap();
        assert!(retry.spec.reopen.is_some());
        let t = h.dispatch(retry.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
        assert!(calls(&fake, "worktree.open").is_empty());
        let created = calls(&fake, "worktree.create");
        assert_eq!(created.len(), 2);
        assert_eq!(created[1]["branch"], format!("pastor/t-{}", retry.id));
        assert_ne!(t.spec.checkout.unwrap().path, checkout.path);
    }

    /// A retry whose old checkout was removed meanwhile gets a new one.
    #[tokio::test(start_paused = true)]
    async fn a_retry_creates_the_worktree_when_the_old_one_is_gone() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let first = h.dispatch(worktree_task(&store).id).await.unwrap();
        fake.worktree_remove(first.workspace_id.as_deref().unwrap(), true)
            .await
            .unwrap();
        wait_for("gone", || !state_of(&store, first.id).is_open()).await;
        store
            .update_task(&mut Task {
                state: TaskState::Failed,
                ..store.get_task(first.id).unwrap().unwrap()
            })
            .unwrap();

        let retry = store.insert_retry(first.id).unwrap();
        let t = h.dispatch(retry.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
        assert!(calls(&fake, "worktree.open").is_empty());
        let created = calls(&fake, "worktree.create");
        assert_eq!(created.len(), 2);
        assert_eq!(created[1]["branch"], format!("pastor/t-{}", retry.id));
    }

    /// A stale task may still have its agent at work in its checkout, so its
    /// retry never reopens it, even on a branch the job names: it gets a
    /// branch and a worktree of its own.
    #[tokio::test(start_paused = true)]
    async fn a_retry_of_a_stale_task_gets_its_own_worktree() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let mut task = worktree_task(&store);
        task.spec.branch = Some("fix/x".into());
        store.update_task(&mut task).unwrap();
        let first = h.dispatch(task.id).await.unwrap();
        store
            .update_task(&mut Task {
                state: TaskState::Stale,
                ..store.get_task(first.id).unwrap().unwrap()
            })
            .unwrap();

        let retry = store.insert_retry(first.id).unwrap();
        let t = h.dispatch(retry.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
        assert!(calls(&fake, "worktree.open").is_empty());
        let created = calls(&fake, "worktree.create");
        assert_eq!(created.len(), 2);
        assert_eq!(created[0]["branch"], "fix/x");
        assert_eq!(created[1]["branch"], format!("pastor/t-{}", retry.id));
    }

    #[tokio::test(start_paused = true)]
    async fn close_closes_the_pane_then_the_row() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let closed = h.close(t.id, false).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert!(closed.finished_at.is_some());
        assert_eq!(state_of(&store, t.id), TaskState::Closed);
        assert_eq!(
            calls(&fake, "pane.close"),
            vec![serde_json::json!({"pane_id": t.pane_id.unwrap()})]
        );
        assert!(calls(&fake, "worktree.remove").is_empty());
        assert!(fake.agents().is_empty());
        assert_eq!(h.snapshot().live, 0);
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            if ev.kind == "task.closed" {
                assert_eq!(ev.task_id, Some(t.id));
                break;
            }
        }
        // Again: nothing left to close, the row stays as it is.
        let again = h.close(t.id, false).await.unwrap();
        assert_eq!(again.updated_at, closed.updated_at);
        assert_eq!(calls(&fake, "pane.close").len(), 1);
    }

    /// `pastor task close` (and so `task done`'s auto-close) stops a task
    /// before any reconcile ever ran: the only `agent.list` reply it will
    /// ever see is the one it reads to find the pane to close. A reported
    /// conversation id must be captured from that same reply, or the closed
    /// task cannot be reopened on it (`task attach`) despite the hook having
    /// reported one.
    #[tokio::test(start_paused = true)]
    async fn close_captures_a_reported_session_before_closing_the_pane() {
        use crate::herdr::{AgentSessionInfo, AgentSessionRefKind};

        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        // No reconcile inside the test, so only the close can record it.
        let settings = MachineSettings {
            reconcile_every: Duration::from_secs(3600),
            ..settings()
        };
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task_of(&store, "agy").id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        assert_eq!(t.spec.session_id, None);
        const CONVERSATION: &str = "9f1e2d3c-4b5a-6978-8f90-123456789abc";
        fake.set_agent_session(
            &pane,
            Some(AgentSessionInfo {
                agent: "agy".into(),
                kind: AgentSessionRefKind::Id,
                source: "agy-hook".into(),
                value: CONVERSATION.into(),
            }),
        );
        let closed = h.close(t.id, false).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(
            closed.spec.session_id.as_deref(),
            Some(CONVERSATION),
            "the conversation id is kept even though reconcile never ran"
        );
        assert_eq!(
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .spec
                .session_id
                .as_deref(),
            Some(CONVERSATION)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn close_with_remove_worktree_removes_it_and_a_dirty_one_is_refused() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let clean = h.dispatch(worktree_task(&store).id).await.unwrap();
        let dirty = h.dispatch(worktree_task(&store).id).await.unwrap();
        fake.set_dirty(dirty.workspace_id.as_deref().unwrap());

        let closed = h.close(clean.id, true).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(
            closed.workspace_id, None,
            "a removed worktree leaves no workspace, so prune may take the row"
        );
        assert_eq!(
            calls(&fake, "worktree.remove"),
            vec![serde_json::json!({"workspace_id": clean.workspace_id.unwrap(), "force": false})]
        );
        assert!(
            closes(&fake).is_empty(),
            "worktree.remove closes the pane itself"
        );

        let err = h.close(dirty.id, true).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("dirty_worktree_requires_force"),
            "{err:#}"
        );
        assert_eq!(
            state_of(&store, dirty.id),
            TaskState::Running,
            "a refusal changes nothing"
        );
        assert_eq!(
            h.snapshot().channel,
            ChannelState::Connected,
            "an API error is not an outage"
        );
        assert_eq!(fake.agents().len(), 1);
    }

    /// A workdir gate that records each `forget`, and places anything.
    #[derive(Default)]
    struct Forgot(std::sync::Mutex<Vec<(String, String)>>);

    impl crate::bundle::workdir_task::Gate for Forgot {
        fn place<'a>(
            &'a self,
            _machine: &'a str,
            _dir: &'a str,
            _what: &'a crate::bundle::workdir_task::Placing,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>>
        {
            Box::pin(async { Ok(true) })
        }

        fn forget(&self, machine: &str, dir: &str) {
            self.0
                .lock()
                .unwrap()
                .push((machine.to_string(), dir.to_string()));
        }
    }

    impl Forgot {
        fn dirs(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(_, d)| d.clone())
                .collect()
        }
    }

    /// `settings` with a `Forgot` gate, and the gate.
    fn forgetting(settings: MachineSettings) -> (MachineSettings, Arc<Forgot>) {
        let gate = Arc::new(Forgot::default());
        let settings = MachineSettings {
            workdir_gate: crate::bundle::workdir_task::GateHandle(Some(gate.clone())),
            ..settings
        };
        (settings, gate)
    }

    /// A task's worktree that goes takes the workdir sets placed in it out
    /// with it; a close that keeps the worktree leaves them.
    #[tokio::test]
    async fn a_removed_worktree_takes_its_workdir_sets_with_it() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (settings, gate) = forgetting(MachineSettings::default());
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let kept = h.dispatch(worktree_task(&store).id).await.unwrap();
        h.close(kept.id, false).await.unwrap();
        assert!(gate.dirs().is_empty(), "the worktree stays");
        let gone = h.dispatch(worktree_task(&store).id).await.unwrap();
        let path = gone.spec.checkout.clone().unwrap().path;
        h.close(gone.id, true).await.unwrap();
        assert_eq!(*gate.0.lock().unwrap(), [("m".to_string(), path)]);
    }

    /// The other ways a close takes a worktree, each taking its sets out:
    /// a shared workspace's task whose checkout is gone already, a workspace
    /// herdr lost (git removes the checkout on the machine), and a paused
    /// task whose kept checkout is opened and removed.
    #[tokio::test(start_paused = true)]
    async fn every_close_that_takes_a_worktree_takes_its_workdir_sets() {
        use crate::herdr::Connector as _;
        let start = async |fake: &FakeHerdr, store: &Arc<Store>| {
            let (settings, gate) = forgetting(MachineSettings::default());
            let (h, events) = spawn_with_settings(fake, store, settings);
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            (h, events, gate)
        };

        // Shared workspace, checkout gone before the close.
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events, gate) = start(&fake, &store).await;
        let t = h
            .dispatch(placed_task(&store, Place::Pastor, true).id)
            .await
            .unwrap();
        let path = t.spec.checkout.clone().unwrap().path;
        fake.remove_checkout(&path).await.unwrap();
        h.close(t.id, true).await.unwrap();
        assert_eq!(gate.dirs(), [path], "shared, gone already");

        // The workspace is lost to herdr: git removes the checkout.
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events, gate) = start(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        let path = t.spec.checkout.clone().unwrap().path;
        fake.close_pane(&t.pane_id.clone().unwrap());
        store.close_task(t.id).unwrap();
        h.close(t.id, true).await.unwrap();
        assert_eq!(fake.removed_checkouts(), std::slice::from_ref(&path));
        assert_eq!(gate.dirs(), [path], "lost workspace");

        // A paused task whose checkout is still on disk.
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events, gate) = start(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        let path = t.spec.checkout.clone().unwrap().path;
        fake.close_pane(&t.pane_id.clone().unwrap());
        let mut row = store.get_task(t.id).unwrap().unwrap();
        row.state = TaskState::Paused;
        store.update_task(&mut row).unwrap();
        let closed = h.close(t.id, true).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(gate.dirs(), [path], "paused, removed through a workspace");
    }

    /// Auto-close's removal of a done task's clean worktree takes its sets
    /// out too.
    #[tokio::test(start_paused = true)]
    async fn auto_close_takes_the_workdir_sets_out_with_the_worktree() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (settings, gate) = forgetting(auto_close_settings());
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = run_to_done(&h, &fake, &store, worktree_task(&store)).await;
        auto_closed(&store, t.id).await;
        assert_eq!(gate.dirs(), [t.spec.checkout.clone().unwrap().path]);
    }

    /// A paused task's close that removes its kept worktree takes the sets
    /// out with it; a plain close leaves them.
    #[tokio::test(start_paused = true)]
    async fn closing_a_paused_task_s_worktree_takes_its_workdir_sets_out() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (settings, gate) = forgetting(MachineSettings::default());
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let paused = |store: &Store| {
            let mut t = worktree_task(store);
            t.state = TaskState::Paused;
            t.machine = Some("m".into());
            t.spec.checkout = Some(Box::new(crate::task::Checkout {
                branch: format!("pastor/t-{}", t.id),
                path: format!("/srv/worktrees/app/t-{}", t.id),
                already_open: false,
            }));
            store.update_task(&mut t).unwrap();
            t
        };
        let kept = paused(&store);
        h.close(kept.id, false).await.unwrap();
        assert!(gate.dirs().is_empty(), "the worktree stays");
        let gone = paused(&store);
        h.close(gone.id, true).await.unwrap();
        assert_eq!(gate.dirs(), [gone.spec.checkout.clone().unwrap().path]);
    }

    /// A pane's shell that has left a build running: `sh` with a background
    /// `sleep`, standing for a `flock cargo test`. Returns the shell and the
    /// pid of the child.
    async fn pane_shell_with_a_build(dir: &std::path::Path) -> (tokio::process::Child, u32) {
        let file = dir.join("child");
        let shell = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 300 & echo $! > {}; wait", file.display()))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if let Some(pid) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                return (shell, pid);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the shell did not start its child");
    }

    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Closing a task used to close its pane and leave a build its agent
    /// had started running. Now what runs under the pane's shell ends
    /// before the pane closes, and the task's events say what.
    #[tokio::test]
    async fn closing_a_task_ends_what_its_agent_left_running() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut shell, build) = pane_shell_with_a_build(tmp.path()).await;
        let fake = FakeHerdr::new();
        fake.run_reaps();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        fake.set_shell_pid(t.pane_id.as_deref().unwrap(), shell.id().unwrap());
        assert!(alive(build));

        let closed = h.close(t.id, false).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        let mut gone = false;
        for _ in 0..50 {
            if !alive(build) {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(gone, "the build outlived its task");
        let _ = shell.kill().await;

        // Asked before the pane closed, and told.
        let order: Vec<String> = fake.requests().into_iter().map(|r| r.method).collect();
        let asked = order.iter().position(|m| m == "pane.process_info").unwrap();
        let closed_at = order.iter().rposition(|m| m == "pane.close").unwrap();
        assert!(asked < closed_at, "{order:?}");
        let mut told = None;
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "task.processes_killed" {
                told = Some(ev);
            }
        }
        let ev = told.expect("a task.processes_killed event");
        assert_eq!(ev.task_id, Some(t.id));
        let detail = ev.detail.unwrap();
        assert_eq!(detail["count"], 1, "{detail}");
        assert_eq!(detail["processes"][0]["pid"], build, "{detail}");
        assert_eq!(detail["processes"][0]["found"], "tree", "{detail}");
        assert_eq!(detail["processes"][0]["forced"], false, "{detail}");
    }

    /// A close that removes the checkout also asks for whatever works in
    /// it, which a build that left the pane's tree is found by; a close that
    /// keeps it asks only for the pane's tree. A pane herdr gives no shell
    /// for and a checkout that stays: nothing is asked.
    #[tokio::test(start_paused = true)]
    async fn the_checkout_is_searched_only_when_it_goes() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let removed = h.dispatch(worktree_task(&store).id).await.unwrap();
        let kept = h.dispatch(worktree_task(&store).id).await.unwrap();
        let plain = h.dispatch(new_task(&store).id).await.unwrap();
        fake.set_shell_pid(removed.pane_id.as_deref().unwrap(), 4001);
        fake.set_shell_pid(kept.pane_id.as_deref().unwrap(), 4002);

        h.close(removed.id, true).await.unwrap();
        let path = removed.spec.checkout.clone().map(|c| c.path);
        let got = fake.reaps();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].root, Some(4001));
        assert!(got[0].cwd.is_some(), "{got:?}");
        assert!(path.is_none() || got[0].cwd == path, "{got:?}");

        h.close(kept.id, false).await.unwrap();
        let got = fake.reaps();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[1].root, Some(4002));
        assert_eq!(got[1].cwd, None, "the checkout stays: only the tree");

        h.close(plain.id, false).await.unwrap();
        assert_eq!(fake.reaps().len(), 2, "no shell and no checkout: no ask");
    }

    fn candidate(pid: u32, found: crate::reap::Found) -> crate::reap::Candidate {
        crate::reap::Candidate {
            pid,
            found,
            start: "Wed Oct 28 04:45:28 2026".into(),
            name: "cargo".into(),
        }
    }

    /// The pids `fake` was asked to end, in order.
    fn ended_pids(fake: &FakeHerdr) -> Vec<u32> {
        fake.ended().iter().flatten().map(|c| c.pid).collect()
    }

    /// A close that herdr refuses (a dirty checkout) changes nothing: the
    /// agent and what it started are left running, since the task is.
    #[tokio::test]
    async fn a_refused_close_ends_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut shell, build) = pane_shell_with_a_build(tmp.path()).await;
        let fake = FakeHerdr::new();
        fake.run_reaps();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_shell_pid(&pane, shell.id().unwrap());
        fake.set_dirty(t.workspace_id.as_deref().unwrap());

        let err = h.close(t.id, true).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("dirty_worktree_requires_force"),
            "{err:#}"
        );
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(alive(build), "the build is still running");
        assert!(fake.ended().is_empty(), "{:?}", fake.ended());
        let _ = shell.kill().await;
        let _ = std::process::Command::new("kill")
            .arg(build.to_string())
            .status();
    }

    /// Auto-close keeps a dirty checkout and closes the pane anyway: what
    /// runs under the pane is ended, what works in the checkout is not.
    #[tokio::test(start_paused = true)]
    async fn auto_close_keeping_a_checkout_does_not_end_what_works_in_it() {
        use crate::reap::Found;
        let fake = FakeHerdr::new();
        fake.set_leftovers(vec![
            candidate(4001, Found::Tree),
            candidate(4002, Found::Cwd),
            candidate(4003, Found::Group),
        ]);
        fake.dirty_worktrees(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = run_to_done(&h, &fake, &store, worktree_task(&store)).await;
        auto_closed(&store, t.id).await;
        assert_eq!(ended_pids(&fake), [4001, 4003]);
    }

    /// What works in the checkout is ended only when the checkout went with
    /// the close, in every placement: a dirty one that herdr refuses, or one
    /// a workspace pastor did not open shows, stays, and so does what works
    /// in it; a clean one that is removed takes it all.
    #[tokio::test(start_paused = true)]
    async fn what_works_in_a_checkout_is_ended_only_if_the_checkout_goes() {
        use crate::reap::Found;
        let leftovers = |n: u32| {
            vec![
                candidate(n, Found::Tree),
                candidate(n + 1, Found::Cwd),
                candidate(n + 2, Found::Group),
            ]
        };
        for place in [Place::Repo, Place::Pastor] {
            // Dirty: refused, the pane (shared) may already be closed.
            let fake = FakeHerdr::new();
            fake.set_leftovers(leftovers(6001));
            fake.dirty_worktrees(true);
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let t = h
                .dispatch(placed_task(&store, place.clone(), true).id)
                .await
                .unwrap();
            let err = h.close(t.id, true).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("dirty_worktree_requires_force"),
                "{place:?}: {err:#}"
            );
            let ended = ended_pids(&fake);
            assert!(!ended.contains(&6002), "{place:?} dirty: {ended:?}");
            assert_eq!(
                ended,
                if place == Place::Pastor {
                    vec![6001, 6003]
                } else {
                    vec![]
                },
                "{place:?} dirty: only a pane already closed ends the rest"
            );

            // A workspace pastor did not open shows it (a task in a shared
            // workspace): kept, with a note.
            if place == Place::Pastor {
                let fake = FakeHerdr::new();
                fake.set_leftovers(leftovers(7001));
                let store = Arc::new(Store::open_in_memory().unwrap());
                let (h, _events) = connected(&fake, &store).await;
                let t = h
                    .dispatch(placed_task(&store, place.clone(), true).id)
                    .await
                    .unwrap();
                let checkout = t.spec.checkout.clone().unwrap();
                fake.worktree_open("/r", &checkout.branch, "mine")
                    .await
                    .unwrap();
                let closed = h.close(t.id, true).await.unwrap();
                let note = closed.error.as_deref().unwrap_or_default();
                assert!(note.contains("worktree kept"), "{closed:?}");
                assert_eq!(ended_pids(&fake), [7001, 7003], "kept: only the pane's");
            }

            // Clean: removed, and what works in it goes with it.
            let fake = FakeHerdr::new();
            fake.set_leftovers(leftovers(8001));
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let t = h
                .dispatch(placed_task(&store, place.clone(), true).id)
                .await
                .unwrap();
            h.close(t.id, true).await.unwrap();
            assert_eq!(ended_pids(&fake), [8001, 8002, 8003], "{place:?} removed");
        }

        // herdr no longer knows the recorded workspace, whose checkout git
        // removes on the machine: it is gone, and what works in it goes.
        let fake = FakeHerdr::new();
        fake.set_leftovers(leftovers(9001));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        fake.forget_workspace(t.workspace_id.as_deref().unwrap());
        h.close(t.id, true).await.unwrap();
        assert_eq!(
            fake.removed_checkouts(),
            vec![t.spec.checkout.clone().unwrap().path],
            "git removed it"
        );
        assert_eq!(
            ended_pids(&fake),
            [9001, 9002, 9003],
            "recorded workspace lost"
        );

        // A task in a shared workspace whose checkout is gone already:
        // nothing is left to remove, and what works there goes.
        let fake = FakeHerdr::new();
        fake.set_leftovers(leftovers(9101));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h
            .dispatch(placed_task(&store, Place::Pastor, true).id)
            .await
            .unwrap();
        fake.remove_checkout(&t.spec.checkout.clone().unwrap().path)
            .await
            .unwrap();
        h.close(t.id, true).await.unwrap();
        assert_eq!(
            ended_pids(&fake),
            [9101, 9102, 9103],
            "checkout already gone"
        );
    }

    /// Every place that closes a task's pane lists what runs under it
    /// first and ends it after: a failed task's pane closing after
    /// `close_failed_after`, an orphan's, and the pane of a task that is
    /// paused.
    #[tokio::test(start_paused = true)]
    async fn the_other_ways_a_pane_closes_end_what_it_left_too() {
        use crate::reap::Found;
        // A failed task's pane.
        let fake = FakeHerdr::new();
        fake.set_leftovers(vec![candidate(5001, Found::Tree)]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let t = stopped_task(&fake, &store, TaskState::Failed).await;
        let pane = t.pane_id.clone().unwrap();
        fake.set_shell_pid(&pane, 777);
        let (_h, _events) = spawn_with_settings(&fake, &store, close_failed_settings());
        wait_for("pane closed", || !pane_open(&fake, &pane)).await;
        assert_eq!(
            fake.reaps(),
            [crate::reap::Spec {
                root: Some(777),
                cwd: None
            }]
        );
        assert_eq!(ended_pids(&fake), [5001]);
        let order: Vec<String> = fake.requests().into_iter().map(|r| r.method).collect();
        let asked = order.iter().position(|m| m == "pane.process_info").unwrap();
        assert!(asked < order.iter().position(|m| m == "pane.close").unwrap());

        // An orphan's pane.
        let fake = FakeHerdr::new();
        fake.set_leftovers(vec![candidate(5002, Found::Tree)]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let ghost = start_agent(&fake, "t-99").await;
        fake.set_shell_pid(&ghost, 778);
        let (_h, _events) = spawn_with_settings(&fake, &store, close_failed_settings());
        wait_for("orphan closed", || !pane_open(&fake, &ghost)).await;
        assert_eq!(ended_pids(&fake), [5002]);

        // A paused task's pane.
        let fake = FakeHerdr::new();
        fake.set_leftovers(vec![candidate(5003, Found::Tree)]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let low = store
            .insert_task_at(
                NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "hi".into(),
                    spec: spec(),
                    flock: "default".into(),
                },
                crate::task::Priority::Low,
                None,
                crate::task::TaskRole::Agent,
            )
            .unwrap();
        let t = h.dispatch(low.id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_shell_pid(&pane, 779);
        fake.set_agent_session(
            &pane,
            Some(crate::herdr::AgentSessionInfo {
                agent: "claude".into(),
                kind: crate::herdr::AgentSessionRefKind::Id,
                source: "claude-hook".into(),
                value: "9f1e2d3c-4b5a-6978-8f90-123456789abc".into(),
            }),
        );
        let other = new_task(&store);
        h.pause(t.id, other.id).await.unwrap();
        assert_eq!(ended_pids(&fake), [5003]);
    }

    /// A row pastor closed already names a pane herdr may have handed to
    /// another agent: closing it again, to remove the checkout, asks for
    /// nothing under that pane.
    #[tokio::test(start_paused = true)]
    async fn a_closed_rows_pane_is_not_searched() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        fake.set_shell_pid(t.pane_id.as_deref().unwrap(), 4001);
        h.close(t.id, false).await.unwrap();
        assert_eq!(fake.reaps().len(), 1);
        h.close(t.id, true).await.unwrap();
        assert_eq!(fake.reaps().len(), 1, "{:?}", fake.reaps());
    }

    fn placed_task(store: &Store, place: Place, worktree: bool) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    repo: Some("/r".into()),
                    worktree,
                    place,
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    /// A task placed as a pane in a workspace it did not make: closing it
    /// closes its pane and nothing else, whatever the place.
    #[tokio::test(start_paused = true)]
    async fn close_leaves_a_workspace_the_task_did_not_make() {
        for (place, worktree) in [
            (Place::Repo, false),
            (Place::Pane("work".into()), false),
            (Place::Pastor, false),
            (Place::Pastor, true),
        ] {
            let fake = FakeHerdr::new();
            fake.open_user_workspace("work", Some("/r"));
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let t = h
                .dispatch(placed_task(&store, place.clone(), worktree).id)
                .await
                .unwrap();
            let ws = t.workspace_id.clone().unwrap();
            let before = fake.panes(&ws);
            assert_eq!(before.len(), 2, "{place}: {before:?}");
            let closed = h.close(t.id, false).await.unwrap();
            assert_eq!(closed.state, TaskState::Closed);
            assert_eq!(
                fake.panes(&ws),
                vec![before[0].clone()],
                "{place}: only the task's pane goes"
            );
            assert!(calls(&fake, "worktree.remove").is_empty());
        }
    }

    /// A fix round started with `--repo` on a worktree task's checkout joins
    /// that task's workspace. Removing the worktree would end the fix
    /// round: auto-close keeps it with a note, `--remove-worktree` refuses,
    /// and either way only the first task's pane closes.
    #[tokio::test(start_paused = true)]
    async fn a_worktree_another_agent_works_in_stays() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let first = h.dispatch(worktree_task(&store).id).await.unwrap();
        let checkout = first.spec.checkout.clone().unwrap();
        let mut fix = new_task(&store);
        fix.spec.repo = Some(checkout.path.clone());
        store.update_task(&mut fix).unwrap();
        let fix = h.dispatch(fix.id).await.unwrap();
        let ws = first.workspace_id.clone().unwrap();
        assert_eq!(fix.workspace_id.as_deref(), Some(ws.as_str()));

        let err = h.close(first.id, true).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("another agent in it, t-2"),
            "{err:#}"
        );

        fake.set_status(first.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        auto_closed(&store, first.id).await;
        assert!(calls(&fake, "worktree.remove").is_empty());
        assert_eq!(fake.panes(&ws), vec![fix.pane_id.clone().unwrap()]);
        let row = store.get_task(first.id).unwrap().unwrap();
        assert!(
            row.error.as_deref().unwrap().contains("t-2 works in it"),
            "{row:?}"
        );
        assert_eq!(row.workspace_id.as_deref(), Some(ws.as_str()));
    }

    /// A worktree task in a shared workspace has no workspace on its
    /// checkout: `--remove-worktree` closes its pane, has herdr open the
    /// checkout and removes that, never the shared workspace.
    #[tokio::test(start_paused = true)]
    async fn remove_worktree_of_a_task_in_a_shared_workspace() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h
            .dispatch(placed_task(&store, Place::Pastor, true).id)
            .await
            .unwrap();
        let pastor = t.workspace_id.clone().unwrap();
        let closed = h.close(t.id, true).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(closed.workspace_id, None);
        let removed = calls(&fake, "worktree.remove");
        assert_eq!(removed.len(), 1);
        assert_ne!(removed[0]["workspace_id"], pastor.as_str());
        assert!(fake.worktree_list("/r").await.unwrap().is_empty());
        assert_eq!(fake.workspaces(), vec![pastor.clone()]);
        assert_eq!(fake.panes(&pastor), vec![format!("{pastor}:p1")]);

        // A plain close first, then the removal: the same, from the row.
        let t = h
            .dispatch(placed_task(&store, Place::Pastor, true).id)
            .await
            .unwrap();
        h.close(t.id, false).await.unwrap();
        assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1);
        let removed = h.close(t.id, true).await.unwrap();
        assert_eq!(removed.workspace_id, None);
        assert!(fake.worktree_list("/r").await.unwrap().is_empty());
        assert_eq!(fake.workspaces(), vec![pastor]);
    }

    /// A worktree task in a shared workspace whose checkout someone else has
    /// open, with an agent in it: `worktree.open` answers that workspace, and
    /// removing it would take the other agent. `--remove-worktree` refuses,
    /// auto-close keeps the checkout with a note, and the other workspace
    /// keeps every pane it had.
    #[tokio::test(start_paused = true)]
    async fn a_shared_task_worktree_another_agent_has_open_stays() {
        for auto in [false, true] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = spawn_with_settings(
                &fake,
                &store,
                if auto {
                    auto_close_settings()
                } else {
                    settings()
                },
            );
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            let t = h
                .dispatch(placed_task(&store, Place::Pastor, true).id)
                .await
                .unwrap();
            let pastor = t.workspace_id.clone().unwrap();
            let checkout = t.spec.checkout.clone().unwrap();
            let other = fake
                .worktree_open("/r", &checkout.branch, "mine")
                .await
                .unwrap();
            let ws = other.workspace.workspace_id.clone();
            fake.agent_start("mine", "claude", &other.root_pane.pane_id, &[])
                .await
                .unwrap();
            let before = fake.panes(&ws);

            if auto {
                fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
                auto_closed(&store, t.id).await;
                let row = store.get_task(t.id).unwrap().unwrap();
                assert!(
                    row.error.as_deref().unwrap().contains("mine works in it"),
                    "{row:?}"
                );
            } else {
                let err = h.close(t.id, true).await.unwrap_err();
                assert!(
                    format!("{err:#}").contains("another agent in it, mine"),
                    "{err:#}"
                );
            }
            assert!(calls(&fake, "worktree.remove").is_empty(), "auto {auto}");
            assert_eq!(fake.panes(&ws), before, "auto {auto}");
            assert!(fake.workspaces().contains(&pastor));
            assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1);
            assert!(
                fake.agents()
                    .iter()
                    .any(|a| a.name.as_deref() == Some("mine"))
            );
        }
    }

    /// A worktree task in a shared workspace whose checkout someone has open
    /// with nobody at work in it: `worktree.open` answers that workspace with
    /// `already_open`, and `worktree.remove` would close it. pastor did not
    /// open it, so the checkout stays with a note to remove it by hand, for
    /// `--remove-worktree` and auto-close alike, and the workspace keeps every
    /// pane it had.
    #[tokio::test(start_paused = true)]
    async fn a_shared_task_worktree_already_open_elsewhere_is_never_removed() {
        for auto in [false, true] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = spawn_with_settings(
                &fake,
                &store,
                if auto {
                    auto_close_settings()
                } else {
                    settings()
                },
            );
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            let t = h
                .dispatch(placed_task(&store, Place::Pastor, true).id)
                .await
                .unwrap();
            let checkout = t.spec.checkout.clone().unwrap();
            let ws = fake
                .worktree_open("/r", &checkout.branch, "mine")
                .await
                .unwrap()
                .workspace
                .workspace_id;
            let before = fake.panes(&ws);

            if auto {
                fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
                auto_closed(&store, t.id).await;
            } else {
                let closed = h.close(t.id, true).await.unwrap();
                assert_eq!(closed.state, TaskState::Closed);
            }
            let row = store.get_task(t.id).unwrap().unwrap();
            let note = row.error.as_deref().unwrap_or_default();
            assert!(note.contains("worktree kept"), "auto {auto}: {row:?}");
            assert!(note.contains("git worktree remove"), "auto {auto}: {row:?}");
            assert!(calls(&fake, "worktree.remove").is_empty(), "auto {auto}");
            assert_eq!(fake.panes(&ws), before, "auto {auto}");
            assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1);
            assert!(
                !fake
                    .agents()
                    .iter()
                    .any(|a| a.pane_id == t.pane_id.clone().unwrap()),
                "auto {auto}: the task's own pane goes"
            );
        }
    }

    /// A checkout can have agents at work in it from outside any workspace
    /// that shows it: a fix round placed in `pastor` works in another task's
    /// checkout from a pane of the shared workspace. `worktree.open` would
    /// make a workspace of its own on it, with nobody listed there, so the
    /// agents are found by the checkout's path instead. Whether the task
    /// with the worktree is itself shared or has its own workspace, the
    /// checkout stays: `--remove-worktree` refuses and names the other
    /// agent, auto-close keeps it with a note, and nothing is opened on it.
    #[tokio::test(start_paused = true)]
    async fn a_worktree_another_agent_works_in_from_a_shared_workspace_stays() {
        for (place, auto) in [
            (Place::Pastor, false),
            (Place::Pastor, true),
            (Place::Own, false),
            (Place::Own, true),
        ] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = spawn_with_settings(
                &fake,
                &store,
                if auto {
                    auto_close_settings()
                } else {
                    settings()
                },
            );
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            let t = h
                .dispatch(placed_task(&store, place.clone(), true).id)
                .await
                .unwrap();
            let checkout = t.spec.checkout.clone().unwrap();
            let mut fix = placed_task(&store, Place::Pastor, false);
            fix.spec.repo = Some(checkout.path.clone());
            store.update_task(&mut fix).unwrap();
            let fix = h.dispatch(fix.id).await.unwrap();
            assert_eq!(fix.state, TaskState::Running, "{:?}", fix.error);
            let workspaces = fake.workspaces();
            let what = format!("{place:?}, auto {auto}");

            if auto {
                fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
                auto_closed(&store, t.id).await;
                let row = store.get_task(t.id).unwrap().unwrap();
                assert!(
                    row.error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("t-2 works in it"),
                    "{what}: {row:?}"
                );
            } else {
                let err = h.close(t.id, true).await.unwrap_err();
                assert!(
                    format!("{err:#}").contains("another agent in it, t-2"),
                    "{what}: {err:#}"
                );
            }
            assert!(calls(&fake, "worktree.remove").is_empty(), "{what}");
            assert!(calls(&fake, "worktree.open").is_empty(), "{what}");
            assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1, "{what}");
            assert!(
                fake.agents()
                    .iter()
                    .any(|a| a.name.as_deref() == Some("t-2")),
                "{what}"
            );
            if !auto || place == Place::Pastor {
                assert_eq!(fake.workspaces(), workspaces, "{what}");
            }
        }
    }

    /// A dispatch can fail after its agent started, and that agent keeps
    /// working: reconcile calls it an orphan, but it may still be in another
    /// task's checkout, from a pane of the shared workspace where no
    /// workspace of the checkout lists it. Its failed row still names it, so
    /// `--remove-worktree` refuses and names it, whatever the place of the
    /// task with the worktree.
    #[tokio::test(start_paused = true)]
    async fn a_failed_task_s_agent_still_in_the_checkout_keeps_it() {
        for place in [Place::Pastor, Place::Own] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let t = h
                .dispatch(placed_task(&store, place.clone(), true).id)
                .await
                .unwrap();
            let checkout = t.spec.checkout.clone().unwrap();
            let mut fix = placed_task(&store, Place::Pastor, false);
            fix.spec.repo = Some(checkout.path.clone());
            store.update_task(&mut fix).unwrap();
            let fix = h.dispatch(fix.id).await.unwrap();
            let mut fix = store.get_task(fix.id).unwrap().unwrap();
            fix.state = TaskState::Failed;
            store.update_task(&mut fix).unwrap();

            let err = h.close(t.id, true).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("another agent in it, t-2"),
                "{place:?}: {err:#}"
            );
            assert!(calls(&fake, "worktree.remove").is_empty(), "{place:?}");
            assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1);
        }
    }

    /// A dirty checkout is refused as for any worktree task, and the
    /// workspace herdr opened to reach it goes again.
    #[tokio::test(start_paused = true)]
    async fn remove_worktree_in_a_shared_workspace_refuses_a_dirty_checkout() {
        let fake = FakeHerdr::new();
        fake.dirty_worktrees(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h
            .dispatch(placed_task(&store, Place::Pastor, true).id)
            .await
            .unwrap();
        let pastor = t.workspace_id.clone().unwrap();
        let err = h.close(t.id, true).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("dirty_worktree_requires_force"),
            "{err:#}"
        );
        assert_eq!(fake.workspaces(), vec![pastor]);
        assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1);
    }

    /// Auto-close removes a clean checkout of a task in `pastor` as it does
    /// for any worktree task, and leaves the `pastor` workspace.
    #[tokio::test(start_paused = true)]
    async fn auto_close_removes_the_worktree_of_a_task_in_a_shared_workspace() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = run_to_done(&h, &fake, &store, placed_task(&store, Place::Pastor, true)).await;
        let pastor = t.workspace_id.clone().unwrap();
        auto_closed(&store, t.id).await;
        assert!(fake.worktree_list("/r").await.unwrap().is_empty());
        assert_eq!(fake.workspaces(), vec![pastor.clone()]);
        assert_eq!(fake.panes(&pastor), vec![format!("{pastor}:p1")]);
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.workspace_id, None);
        assert!(row.error.is_none(), "{:?}", row.error);
    }

    #[tokio::test(start_paused = true)]
    async fn remove_worktree_on_a_plain_workspace_is_refused_up_front() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let err = h.close(t.id, true).await.unwrap_err();
        assert!(err.to_string().contains("no worktree"), "{err}");
        assert!(calls(&fake, "pane.close").is_empty());
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// `task close` is how an orphan goes: a failed row whose agent is still
    /// alive, or an agent with no row at all.
    #[tokio::test(start_paused = true)]
    async fn close_takes_orphans_by_agent_name() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut failed = new_task(&store);
        let pane = start_agent(&fake, &Task::agent_name_for(failed.id)).await;
        failed.state = TaskState::Failed;
        failed.machine = Some("m".into());
        failed.finished_at = Some(Utc::now());
        store.update_task(&mut failed).unwrap();
        let ghost = start_agent(&fake, "t-99").await;
        let (h, _events) = connected(&fake, &store).await;
        wait_for("orphans", || h.snapshot().orphans.len() == 2).await;

        let closed = h.close(failed.id, false).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(closed.finished_at, failed.finished_at);
        let err = h.close(99, false).await.unwrap_err();
        let orphan = err.downcast_ref::<OrphanClosed>().expect("an OrphanClosed");
        assert_eq!(orphan.agent, "t-99");
        let closed_panes: Vec<_> = calls(&fake, "pane.close")
            .iter()
            .map(|p| p["pane_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(closed_panes, vec![pane, ghost]);
        assert!(h.snapshot().orphans.is_empty());
        assert_eq!(h.snapshot().live, 0);

        let err = h.close(99, false).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    /// `task close` has the same guard: a failed task whose recorded pane now
    /// holds another agent closes its row and leaves that pane and its
    /// workspace alone.
    #[tokio::test(start_paused = true)]
    async fn close_leaves_a_pane_reused_by_another_agent() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut failed = worktree_task(&store);
        let other = start_agent(&fake, "t-99").await;
        failed.state = TaskState::Failed;
        failed.machine = Some("m".into());
        failed.workspace_id = Some(other.split(':').next().unwrap().into());
        failed.pane_id = Some(other.clone());
        failed.agent_name = Some(Task::agent_name_for(failed.id));
        failed.finished_at = Some(Utc::now());
        store.update_task(&mut failed).unwrap();
        let (h, _events) = connected(&fake, &store).await;

        let err = h.close(failed.id, true).await.unwrap_err();
        assert!(err.to_string().contains("git worktree remove"), "{err}");
        assert!(calls(&fake, "worktree.remove").is_empty());
        let closed = h.close(failed.id, false).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert!(calls(&fake, "pane.close").is_empty());
        assert_eq!(fake.agents().len(), 1);
        assert_eq!(fake.agents()[0].pane_id, other);
    }

    /// A dispatch that fails at `agent.start` records the workspace and pane
    /// and leaves them open with no agent in them. Close must use those ids:
    /// looking for an agent by name finds nothing and would leak the pane,
    /// and refuse `--remove-worktree`.
    #[tokio::test(start_paused = true)]
    async fn close_uses_the_recorded_pane_of_a_failed_task_with_no_agent() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        fake.set_start_behaviour(crate::herdr::fake::StartBehaviour::Fail("boom".into()));
        let plain = new_task(&store).id;
        let wt = worktree_task(&store).id;
        let _ = h.dispatch(plain).await;
        let _ = h.dispatch(wt).await;
        let plain = store.get_task(plain).unwrap().unwrap();
        let wt = store.get_task(wt).unwrap().unwrap();
        for t in [&plain, &wt] {
            assert_eq!(t.state, TaskState::Failed, "{t:?}");
            assert!(t.pane_id.is_some() && t.workspace_id.is_some(), "{t:?}");
        }
        assert!(fake.agents().is_empty());
        assert_eq!(
            fake.workspaces().len(),
            2,
            "the failed dispatch left both open"
        );

        let closed = h.close(plain.id, false).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(
            closes(&fake),
            vec![serde_json::json!({"pane_id": plain.pane_id.clone().unwrap()})]
        );
        let closed = h.close(wt.id, true).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(
            calls(&fake, "worktree.remove"),
            vec![
                serde_json::json!({"workspace_id": wt.workspace_id.clone().unwrap(), "force": false})
            ]
        );
        assert!(fake.workspaces().is_empty());
    }

    /// A closed row can still record a workspace whose pane never went
    /// through pastor's own close: `--remove-worktree` must use that
    /// recorded id rather than fall back to a name lookup that finds
    /// nothing and leaves the checkout behind.
    #[tokio::test(start_paused = true)]
    async fn close_remove_worktree_uses_the_recorded_workspace_of_a_closed_row() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let workspace = t.workspace_id.clone().unwrap();
        // The pane disappears without going through pastor's close, and the
        // row is marked closed without reaching herdr: the workspace is
        // still recorded, but a name lookup would find no agent.
        fake.exit_pane(&pane);
        store.close_task(t.id).unwrap();
        assert_eq!(state_of(&store, t.id), TaskState::Closed);

        let closed = h.close(t.id, true).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(
            calls(&fake, "worktree.remove"),
            vec![serde_json::json!({"workspace_id": workspace, "force": false})]
        );
        assert!(fake.workspaces().is_empty());
    }

    /// The recorded workspace can be gone from herdr entirely by the time
    /// `--remove-worktree` reaches it -- closed some other way, say. herdr
    /// has nothing to remove the checkout by, so git removes it on the
    /// machine, and the close succeeds with no note; a dirty checkout stays,
    /// noted, and the row closes all the same.
    #[tokio::test(start_paused = true)]
    async fn close_remove_worktree_handles_a_recorded_workspace_herdr_has_lost() {
        for dirty in [false, true] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let t = h.dispatch(worktree_task(&store).id).await.unwrap();
            let pane = t.pane_id.clone().unwrap();
            let path = t.spec.checkout.clone().unwrap().path;
            if dirty {
                fake.set_dirty_checkout(&path);
            }
            // herdr has lost the workspace and its pane both, without going
            // through pastor's close, and the row is marked closed without
            // reaching herdr: the workspace id is still recorded, but herdr no
            // longer has it, so `worktree.remove` answers `workspace_not_found`.
            fake.close_pane(&pane);
            store.close_task(t.id).unwrap();
            assert_eq!(state_of(&store, t.id), TaskState::Closed);

            let closed = h.close(t.id, true).await.unwrap();
            assert_eq!(closed.state, TaskState::Closed);
            assert_eq!(
                fake.removed_checkouts(),
                vec![path.clone()],
                "dirty {dirty}"
            );
            let note = closed.error.clone().unwrap_or_default();
            assert_eq!(
                note.contains("worktree kept: uncommitted or untracked files"),
                dirty,
                "{note}"
            );
            assert_eq!(
                closed.workspace_id, None,
                "herdr has nothing left to remove; git or the note carries the rest"
            );
        }
    }

    /// A plain close only closes the pane (herdr closes the workspace with
    /// its last pane) and leaves the checkout on disk, so the row keeps its
    /// workspace and prune will not take it. `--remove-worktree` afterwards
    /// finds the workspace gone, removes the checkout with git and clears
    /// the workspace; running it again has nothing left to do.
    #[tokio::test(start_paused = true)]
    async fn close_keeps_the_workspace_until_the_worktree_is_dealt_with() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        let workspace = t.workspace_id.clone().unwrap();

        let closed = h.close(t.id, false).await.unwrap();
        assert_eq!(closed.workspace_id.as_deref(), Some(workspace.as_str()));

        let removed = h.close(t.id, true).await.unwrap();
        assert_eq!(removed.state, TaskState::Closed);
        assert_eq!(removed.workspace_id, None);
        assert_eq!(removed.error, None);
        assert_eq!(
            fake.removed_checkouts(),
            vec![t.spec.checkout.clone().unwrap().path]
        );

        let again = h.close(t.id, true).await.unwrap();
        assert_eq!(again.workspace_id, None);
        assert_eq!(again.updated_at, removed.updated_at, "nothing changed");
        assert_eq!(calls(&fake, "worktree.remove").len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn close_without_herdr_work() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        // Queued, never dispatched: only the row changes.
        let queued = new_task(&store);
        assert_eq!(
            h.close(queued.id, false).await.unwrap().state,
            TaskState::Closed
        );
        // Its pane already gone behind pastor's back: still closes the row.
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let mut gone = store.get_task(t.id).unwrap().unwrap();
        gone.pane_id = Some("w77:p1".into());
        store.update_task(&mut gone).unwrap();
        assert_eq!(h.close(t.id, false).await.unwrap().state, TaskState::Closed);
        // Another machine's task is not this machine's to close.
        let mut other = new_task(&store);
        other.state = TaskState::Running;
        other.machine = Some("elsewhere".into());
        other.pane_id = Some("w1:p1".into());
        store.update_task(&mut other).unwrap();
        let err = h.close(other.id, false).await.unwrap_err();
        assert!(err.to_string().contains("elsewhere"), "{err}");
        assert_eq!(state_of(&store, other.id), TaskState::Running);
    }

    /// Settings for the auto-close tests: done tasks are closed 150ms after
    /// they finish, checked by a reconcile every 100ms.
    fn input_events(events: &mut broadcast::Receiver<PastorEvent>) -> Vec<PastorEvent> {
        let mut out = vec![];
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "task.input" {
                out.push(ev);
            }
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn send_types_text_then_enter_then_keys_into_the_task_pane() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        h.send(
            t.id,
            SendInput {
                text: Some("my secret".into()),
                enter: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        h.send(
            t.id,
            SendInput {
                keys: vec!["esc".into(), "Down".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        h.send(
            t.id,
            SendInput {
                text: Some("half".into()),
                enter: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fake.pane_input(&pane),
            [
                PaneInput::Text("my secret".into()),
                PaneInput::Keys(vec!["Enter".into()]),
                PaneInput::Keys(vec!["esc".into(), "Down".into()]),
                PaneInput::Text("half".into()),
            ]
        );
        let evs = input_events(&mut events);
        assert_eq!(evs.len(), 3, "{evs:?}");
        assert!(evs.iter().all(|e| e.task_id == Some(t.id)));
        // The text itself may be a secret: only its length is recorded.
        assert_eq!(
            evs[0].detail,
            Some(serde_json::json!({"text_len": 9, "keys": ["Enter"]}))
        );
        assert_eq!(
            evs[1].detail,
            Some(serde_json::json!({"keys": ["esc", "Down"]}))
        );
        assert_eq!(evs[2].detail, Some(serde_json::json!({"text_len": 4})));
        assert!(!format!("{evs:?}").contains("secret"));
    }

    fn repo_task(store: &Store, agent: &str, repo: Option<&str>) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    agent: agent.into(),
                    repo: repo.map(str::to_string),
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    fn trust() -> SendInput {
        SendInput {
            trust: true,
            ..Default::default()
        }
    }

    /// The folder-trust dialog answered by hand: `--trust` sends the agent's
    /// trust keys, the agent goes on to get its prompt, and the repo is saved
    /// as trusted on this machine.
    #[tokio::test(start_paused = true)]
    async fn send_trust_answers_the_prompt_and_saves_the_repo() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h
            .dispatch(repo_task(&store, "claude", Some("~/src/app")).id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        h.send(t.id, trust()).await.unwrap();
        // `Down` goes on its own, the pane is read to confirm it landed,
        // and only then does `Enter` confirm the selection.
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [
                PaneInput::Keys(vec!["Down".into()]),
                PaneInput::Keys(vec!["Enter".into()]),
            ]
        );
        assert!(store.is_trusted("m", "~/src/app").unwrap());
        wait_for("the pending prompt delivered", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        let evs = input_events(&mut events);
        assert_eq!(
            evs[0].detail,
            Some(serde_json::json!({"keys": ["Down", "Enter"], "trust": true}))
        );
    }

    /// The bug `--trust` exists to fix: a `Down` lost on the way to
    /// a real pane leaves Claude's dialog on "No, exit". `--trust` reads the
    /// pane after `Down` and refuses to press `Enter` while it is not on
    /// "Yes, I trust this folder", so it fails instead of quitting Claude
    /// while still marking the repo trusted.
    #[tokio::test(start_paused = true)]
    async fn send_trust_refuses_to_confirm_a_lost_down_key() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h
            .dispatch(repo_task(&store, "claude", Some("~/src/app")).id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        fake.lose_next_trust_keys(t.pane_id.as_deref().unwrap(), 1);
        let err = h.send(t.id, trust()).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<SendRefused>().map(|r| r.code),
            Some("trust_not_selected"),
            "{err:#}"
        );
        // `Down` went, and was lost; `Enter` never did, so Claude was never
        // left to exit on "No, exit".
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [PaneInput::Keys(vec!["Down".into()])]
        );
        assert!(!store.is_trusted("m", "~/src/app").unwrap());
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(input_events(&mut events).is_empty());
    }

    /// Claude redraws for a moment after its trust dialog, and loses a prompt
    /// typed then although herdr accepts it. Settings where that moment is
    /// shorter than the settle window, and the fake set to lose such prompts.
    fn redrawing_after_trust() -> (FakeHerdr, MachineSettings) {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        fake.set_trust_redraw(Duration::from_millis(250));
        let settings = MachineSettings {
            settle: Duration::from_millis(400),
            ..settings()
        };
        (fake, settings)
    }

    /// The agent has the prompt: it went to work on it.
    fn agent_working(fake: &FakeHerdr) -> bool {
        fake.agents()
            .first()
            .is_some_and(|a| a.agent_status == AgentStatus::Working)
    }

    /// `--trust` on an agent that redraws after its dialog: the prompt waits
    /// out the settle window, so it reaches the agent, and goes in once.
    #[tokio::test(start_paused = true)]
    async fn send_trust_delivers_the_prompt_once_after_the_agent_redraws() {
        let (fake, settings) = redrawing_after_trust();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h
            .dispatch(repo_task(&store, "claude", Some("~/src/app")).id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        h.send(t.id, trust()).await.unwrap();
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        // Some reconciles later, still once.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// The same when a person answers the dialog at the pane: pastor sees
    /// only the agent leave `blocked`, and still waits out the redraw.
    #[tokio::test(start_paused = true)]
    async fn a_trust_prompt_answered_at_the_pane_gets_the_prompt_after_the_redraw() {
        let (fake, settings) = redrawing_after_trust();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h
            .dispatch(repo_task(&store, "claude", Some("~/src/app")).id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        fake.answer_trust_by_hand(t.pane_id.as_deref().unwrap());
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// Codex's folder-trust dialog, as a new folder shows it.
    const CODEX_TRUST_SCREEN: &str = "> You are in /home/fake/src/app\n\n  Do you trust the contents of this directory? Working with untrusted\n  contents comes with higher risk of prompt injection.\n\n\u{203a} 1. Yes, continue\n  2. No, quit\n\n  Press enter to continue\n";

    /// Codex asks whether to trust a new folder, and herdr reads that
    /// dialog idle and ready, not blocked: a prompt sent then goes into the
    /// dialog and is lost. pastor reads the pane first, holds the prompt
    /// with the task blocked, and sends it once a person has answered.
    #[tokio::test(start_paused = true)]
    async fn a_codex_task_in_a_new_folder_gets_its_prompt_after_the_trust_answer() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Enter".into()]));
        fake.set_trust_idle(true);
        fake.set_trust_screen(CODEX_TRUST_SCREEN);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_of(&store, "codex").id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.prompt_pending);
        assert_eq!(calls(&fake, "agent.prompt").len(), 0);
        // Reconciles and settle windows go by: still at the dialog.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 0);
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);

        fake.answer_trust_by_hand(t.pane_id.as_deref().unwrap());
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// agy's folder-trust dialog in a new folder, as agy 1.4.12 draws it
    /// (captured, the folder replaced). herdr reads it idle and ready.
    const AGY_TRUST_SCREEN: &str = "Accessing workspace:\n\n/home/fake/src/app\n\nDo you trust the contents of this project?\n\nAntigravity CLI requires permission to read, edit, and execute files here.\n\n> Yes, I trust this folder\n  No, exit\n\n  \u{2191}/\u{2193} Navigate \u{b7} enter Confirm\n                                                                                  accept-edits \u{b7} Gemini 4 Argon \u{b7} medium\n";

    /// agy's input once trusted, under a new-model banner. The input is as
    /// agy 1.4.12 draws it (captured, the account and folder replaced); the
    /// banner's line is not a capture, only the text a `dialogs` row for
    /// it names.
    const AGY_BANNER_SCREEN: &str = "\n      \u{2584}\u{2580}\u{2580}\u{2584}        Antigravity CLI 1.4.12\n     \u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}       someone@example.com\n    \u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}      Gemini 4 Argon (Medium)\n   \u{2584}\u{2580}\u{2580}    \u{2580}\u{2580}\u{2584}     ~/src/app\n  \u{2584}\u{2580}\u{2580}      \u{2580}\u{2580}\u{2584}\n\n  Meet Gemini 4 Argon \u{b7} esc to dismiss\n\n────────────────────────────────────────────────────────────\n> Accept-edits mode: file edits auto-approved (shift+tab to cycle)\n────────────────────────────────────────────────────────────\n                                                                                  accept-edits \u{b7} Gemini 4 Argon \u{b7} medium\n";

    /// A bare `agy` agent in a new folder. herdr reads its trust dialog idle
    /// and ready, so a prompt typed then is lost, and the Enter after it
    /// takes the option the dialog opens on, "Yes, I trust this folder":
    /// the folder trusted with nobody asked. pastor knows the dialog: the
    /// prompt waits with the task blocked, and goes in after `--trust`.
    #[tokio::test(start_paused = true)]
    async fn a_bare_agy_task_in_a_new_folder_waits_at_its_trust_dialog() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(AGY_TRUST_SCREEN.into(), keys(&["Enter"]))]);
        fake.set_trust_idle(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h
            .dispatch(repo_task(&store, "agy", Some("~/src/app")).id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(calls(&fake, "agent.prompt").is_empty());
        let pane = t.pane_id.clone().unwrap();
        assert!(fake.pane_input(&pane).is_empty(), "nothing pressed for it");

        h.send(t.id, trust()).await.unwrap();
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert_eq!(fake.pane_input(&pane), [PaneInput::Keys(keys(&["Enter"]))]);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// agy's lone trust key takes whatever option is selected, so `--trust`
    /// reads the dialog first and presses nothing when it is not on "Yes,
    /// I trust this folder".
    #[tokio::test(start_paused = true)]
    async fn agy_trust_is_not_pressed_on_another_option() {
        let fake = FakeHerdr::new();
        let on_no = AGY_TRUST_SCREEN
            .replace("> Yes, I trust", "  Yes, I trust")
            .replace("  No, exit", "> No, exit");
        fake.set_startup_dialogs(vec![(on_no, keys(&["Enter"]))]);
        fake.set_trust_idle(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h
            .dispatch(repo_task(&store, "agy", Some("~/src/app")).id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        let err = h.send(t.id, trust()).await.unwrap_err();
        assert!(err.to_string().contains("not on"), "{err}");
        assert!(fake.pane_input(t.pane_id.as_deref().unwrap()).is_empty());
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
    }

    /// An agy agent with its trust dialog and a banner row of its own.
    fn agy_with_a_banner() -> MachineSettings {
        let mut settings = settings();
        settings.agents = toml::from_str(
            "[agy-work]\nkind = \"agy\"\ntrust_keys = [\"Enter\"]\ntrust_marker = \"Do you trust the contents of this project\"\n[[agy-work.dialogs]]\nname = \"model-banner\"\nmarker = \"esc to dismiss\"\nkeys = [\"esc\"]\n",
        )
        .unwrap();
        settings
    }

    /// agy in a new folder: its trust dialog, then a banner, both read idle
    /// and ready by herdr, so a prompt typed at either is lost. `--trust`
    /// answers the first; the prompt then waits while the banner shows,
    /// pastor answers the banner's row, and the prompt goes in once.
    #[tokio::test(start_paused = true)]
    async fn an_agy_prompt_waits_for_its_banner_after_the_trust_answer() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (AGY_TRUST_SCREEN.into(), keys(&["Enter"])),
            (AGY_BANNER_SCREEN.into(), keys(&["esc"])),
        ]);
        fake.set_trust_idle(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected_with(&fake, &store, agy_with_a_banner()).await;
        let t = h
            .dispatch(repo_task(&store, "agy-work", Some("~/src/app")).id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(calls(&fake, "agent.prompt").is_empty());

        h.send(t.id, trust()).await.unwrap();
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [
                PaneInput::Keys(keys(&["Enter"])),
                PaneInput::Keys(keys(&["esc"])),
            ]
        );
        assert!(store.answered(t.id, "model-banner").unwrap());
    }

    /// The banner without a trust dialog before it, in a folder agy
    /// already trusts: the first prompt waits for it too, at dispatch.
    #[tokio::test(start_paused = true)]
    async fn an_agy_prompt_waits_for_its_banner_at_dispatch() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(AGY_BANNER_SCREEN.into(), keys(&["esc"]))]);
        fake.set_trust_idle(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected_with(&fake, &store, agy_with_a_banner()).await;
        let t = h
            .dispatch(new_task_of(&store, "agy-work").id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(calls(&fake, "agent.prompt").is_empty());
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// An agent that sits at an empty input after its prompt went in (herdr
    /// accepted it, the agent lost it) gets the prompt again once it has
    /// sat idle a settle window at the sequence the prompt went in at.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_the_agent_did_not_take_is_sent_again() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        fake.ignore_prompts(false);
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 2);
        let t = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert!(!t.prompt_pending);
    }

    /// Blocks `fake`'s first agent as soon as it is started, so dispatch
    /// leaves the prompt pending.
    fn block_on_start(fake: &FakeHerdr) {
        fake.set_ready_after(Duration::from_millis(150));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
    }

    /// A resumed task whose start hits a dialog holds the prompt that start
    /// meant: once the dialog clears, a paused task gets `RESUME_PROMPT`
    /// and one back from a usage limit `LIMIT_RESUME_PROMPT`, once, never
    /// the task's own prompt, which its session already ran.
    #[tokio::test(start_paused = true)]
    async fn a_resume_held_at_a_dialog_sends_the_resume_prompt() {
        for (state, want) in [
            (TaskState::Paused, crate::task::RESUME_PROMPT),
            (TaskState::Waiting, crate::task::LIMIT_RESUME_PROMPT),
        ] {
            let fake = FakeHerdr::new();
            block_on_start(&fake);
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let mut t = new_task(&store);
            t.state = state;
            t.machine = Some("m".into());
            t.spec.session_id = crate::task::new_session_id();
            if state == TaskState::Waiting {
                t.waiting_until = Some(Utc::now() - chrono::Duration::minutes(1));
            }
            store.update_task(&mut t).unwrap();
            let t = h.resume(t.id).await.unwrap();
            assert_eq!(t.state, TaskState::Blocked, "{state:?}");
            assert!(calls(&fake, "agent.prompt").is_empty(), "{state:?}");

            fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
            wait_for("the held prompt sent", || {
                let t = store.get_task(t.id).unwrap().unwrap();
                t.state == TaskState::Running && !t.prompt_pending
            })
            .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            let typed: Vec<String> = calls(&fake, "agent.prompt")
                .iter()
                .map(|p| p["text"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(typed, [want], "{state:?}");
        }
    }

    /// A prompt held for a blocked Claude agent goes through its session's
    /// inbox socket once the block clears, and is not typed. The baseline
    /// is the `agent.list` read before the send; the agent's own move to
    /// working is the activity, and its idle after that is done.
    #[tokio::test(start_paused = true)]
    async fn a_pending_prompt_goes_through_the_inbox_socket() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        block_on_start(&fake);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(fake.inbox_sends().is_empty(), "nothing sent while blocked");

        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        let idle_seq = fake.agents()[0].state_change_seq;
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert!(calls(&fake, "agent.prompt").is_empty(), "not typed");
        let sends = fake.inbox_sends();
        assert_eq!(sends.len(), 1);
        assert_eq!(
            Some(sends[0].session.as_str()),
            t.spec.session_id.as_deref()
        );
        assert_eq!(sends[0].text, crate::task::prompt_to_send(&t));
        let got = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(got.last_completion_seq, Some(idle_seq));
        assert!(agent_working(&fake));

        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(saw(&mut events, "task.done", t.id));
        assert_eq!(fake.inbox_sends().len(), 1, "sent once");
    }

    /// A socket send the agent did not act on is sent again, marked with
    /// its attempt, so Claude's guard never drops it as the same text from
    /// the same sender.
    #[tokio::test(start_paused = true)]
    async fn a_socket_prompt_the_agent_did_not_take_is_sent_again_marked() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(fake.inbox_sends().len(), 1);
        fake.ignore_prompts(false);
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        let sends = fake.inbox_sends();
        assert_eq!(sends.len(), 2);
        assert!(calls(&fake, "agent.prompt").is_empty(), "not typed");
        assert_ne!(sends[0].text, sends[1].text);
        assert!(
            sends[1].text.starts_with(&sends[0].text),
            "{:?}",
            sends[1].text
        );
        assert!(sends[1].text.contains("attempt 2"), "{:?}", sends[1].text);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// A pending prompt the socket does not take is typed, as before.
    #[tokio::test(start_paused = true)]
    async fn a_pending_prompt_the_socket_does_not_take_is_typed() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("held"));
        block_on_start(&fake);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(fake.inbox_sends().len(), 1);
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert!(agent_working(&fake));
    }

    /// The `agent.list` read before a socket send gates it as herdr gates
    /// a typed prompt: an agent still launching gets nothing, and the
    /// prompt stays pending until it is up.
    #[tokio::test(start_paused = true)]
    async fn a_launching_agent_gets_no_socket_send() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let ready_after = Duration::from_millis(600);
        fake.set_ready_after(ready_after);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake
            .workspace_create(None, &name, &Default::default())
            .await
            .unwrap();
        let pane = created.root_pane.pane_id.clone();
        let session = crate::task::new_session_id().unwrap();
        let args = ["--session-id".to_string(), session.clone()];
        fake.agent_start(&name, "claude", &pane, &args)
            .await
            .unwrap();
        let launched = Instant::now();
        fake.set_status_silently(&pane, AgentStatus::Unknown);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        t.spec.session_id = Some(session.clone());
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("adopted", || {
            store.get_task(t.id).unwrap().unwrap().pane_id.is_some()
        })
        .await;
        // An idle event while herdr still lists it launching.
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(launched.elapsed() < ready_after);
        assert!(
            fake.inbox_sends().is_empty(),
            "nothing sent while launching"
        );
        assert!(store.get_task(t.id).unwrap().unwrap().prompt_pending);

        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(fake.inbox_sends().len(), 1);
        assert_eq!(fake.inbox_sends()[0].session, session);
        assert!(calls(&fake, "agent.prompt").is_empty());
    }

    /// `task send` text and its Enter go through a Claude session's inbox
    /// socket and are not typed; `task.input` says so. Keys of their own,
    /// and a line sent without Enter, stay keystrokes. The orchestrator's
    /// last message is such a send (text and Enter), so it goes the same
    /// way.
    #[tokio::test(start_paused = true)]
    async fn send_text_goes_through_the_inbox_socket() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        assert_eq!(fake.inbox_sends().len(), 1, "the first prompt");
        while events.try_recv().is_ok() {}
        let text = |text: &str, enter| SendInput {
            text: Some(text.into()),
            enter,
            ..Default::default()
        };
        h.send(t.id, text("next step", true)).await.unwrap();
        let sends = fake.inbox_sends();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[1].text, "next step");
        assert_eq!(
            Some(sends[1].session.as_str()),
            t.spec.session_id.as_deref()
        );
        assert!(fake.pane_input(&pane).is_empty(), "nothing typed");
        let input = loop {
            let ev = events.try_recv().expect("a task.input");
            if ev.kind == "task.input" {
                break ev.detail.unwrap();
            }
        };
        assert_eq!(input["socket"], true, "{input}");
        assert_eq!(input["text_len"], 9, "{input}");

        h.send(
            t.id,
            SendInput {
                keys: vec!["esc".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        h.send(t.id, text("half", false)).await.unwrap();
        assert_eq!(
            fake.pane_input(&pane),
            [
                PaneInput::Keys(vec!["esc".into()]),
                PaneInput::Text("half".into()),
            ]
        );
        assert_eq!(fake.inbox_sends().len(), 2);
    }

    /// Text the socket does not take is typed, as is text for an agent
    /// herdr lists as blocked, whose dialog may be what it answers.
    #[tokio::test(start_paused = true)]
    async fn send_text_the_socket_does_not_take_is_typed() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_inbox_result(Some("held"));
        while events.try_recv().is_ok() {}
        let send = |text: &str| SendInput {
            text: Some(text.into()),
            enter: true,
            ..Default::default()
        };
        h.send(t.id, send("next step")).await.unwrap();
        assert_eq!(fake.inbox_sends().len(), 2, "tried the socket");
        let input = loop {
            let ev = events.try_recv().expect("a task.input");
            if ev.kind == "task.input" {
                break ev.detail.unwrap();
            }
        };
        assert!(input.get("socket").is_none(), "{input}");

        fake.set_inbox_result(Some("accepted"));
        fake.set_status(&pane, AgentStatus::Blocked);
        h.send(t.id, send("2")).await.unwrap();
        assert_eq!(fake.inbox_sends().len(), 2, "no socket send while blocked");
        assert_eq!(
            fake.pane_input(&pane),
            [
                PaneInput::Text("next step".into()),
                PaneInput::Keys(vec!["Enter".into()]),
                PaneInput::Text("2".into()),
                PaneInput::Keys(vec!["Enter".into()]),
            ]
        );
    }

    /// Text that reopens a done task goes through the socket with the
    /// summary ask on its line, and the task runs again.
    #[tokio::test(start_paused = true)]
    async fn send_to_a_done_task_goes_through_the_socket_and_runs_it_again() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = run_to_done(&h, &fake, &store, new_task(&store)).await;
        let pane = t.pane_id.clone().unwrap();
        fake.set_inbox_result(Some("accepted"));
        let sent = h
            .send(
                t.id,
                SendInput {
                    text: Some("commit and push".into()),
                    enter: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(sent.state, TaskState::Running);
        assert_eq!(
            fake.inbox_sends().last().unwrap().text,
            format!("commit and push {}", crate::task::SUMMARY_ASK)
        );
        assert!(fake.pane_input(&pane).is_empty(), "nothing typed");
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// A resumed task's prompt is watched the same way: an agent that lost
    /// `RESUME_PROMPT` gets it again.
    #[tokio::test(start_paused = true)]
    async fn a_resume_prompt_the_agent_did_not_take_is_sent_again() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let mut t = new_task(&store);
        t.state = TaskState::Paused;
        t.machine = Some("m".into());
        t.spec.session_id = crate::task::new_session_id();
        store.update_task(&mut t).unwrap();
        let t = h.resume(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        fake.ignore_prompts(false);
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 2);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// An agent that never takes its prompt is not left `running` with
    /// nothing pending: after the resends it is `blocked`, saying so.
    #[tokio::test(start_paused = true)]
    async fn an_agent_that_never_takes_its_prompt_ends_blocked() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        assert_eq!(
            calls(&fake, "agent.prompt").len(),
            1 + MAX_PROMPT_RESENDS as usize
        );
        let t = store.get_task(t.id).unwrap().unwrap();
        assert!(
            t.error.as_deref().unwrap_or("").contains("did not take"),
            "{:?}",
            t.error
        );
        assert!(saw(&mut events, "task.blocked", t.id));
        // Left for a human: no more prompts.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            calls(&fake, "agent.prompt").len(),
            1 + MAX_PROMPT_RESENDS as usize
        );
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
    }

    /// Codex takes a prompt as a bracketed paste and reads the Enter that
    /// follows as a newline, so it sits in the input box, short as it is or
    /// as a placeholder when long. The agent stays idle: pastor presses Enter
    /// once, as a person would, instead of pasting the prompt again.
    #[tokio::test(start_paused = true)]
    async fn a_codex_prompt_left_in_its_input_is_submitted_not_pasted_again() {
        let codex_input = |text: &str| {
            format!("  gpt-5 default · ~/src/app\n\n› {text}\n\n  ⏎ send · ctrl+j newline\n")
        };
        let long = codex_input("[Pasted Content 4096 chars]");
        let probe = new_task_of(&Store::open_in_memory().unwrap(), "codex");
        let short = codex_input(&crate::task::prompt_to_send(&probe));
        for screen in [long, short] {
            let fake = FakeHerdr::new();
            fake.ignore_prompts(true);
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, mut events) = connected(&fake, &store).await;
            let t = h.dispatch(new_task_of(&store, "codex").id).await.unwrap();
            let pane = t.pane_id.clone().unwrap();
            fake.set_pane_text(&pane, &screen);
            wait_for("Enter pressed", || {
                fake.pane_input(&pane) == [PaneInput::Keys(keys(&["Enter"]))]
            })
            .await;
            assert_eq!(
                calls(&fake, "agent.prompt").len(),
                1,
                "not pasted again: {screen}"
            );
            assert!(saw(&mut events, "task.input", t.id));
            // It took the Enter and is at work.
            fake.set_status(&pane, AgentStatus::Working);
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert_eq!(state_of(&store, t.id), TaskState::Running);
            assert_eq!(fake.pane_input(&pane).len(), 1, "Enter once: {screen}");
        }
    }

    /// An Enter that does not start work is pressed no more often than a
    /// prompt would be sent again, then the task is blocked for a person,
    /// the prompt having gone in once.
    #[tokio::test(start_paused = true)]
    async fn an_enter_codex_ignores_is_pressed_twice_then_the_task_blocks() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_of(&store, "codex").id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_pane_text(&pane, "  gpt-5 default\n\n› [Pasted Content 4096 chars]\n");
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            fake.pane_input(&pane),
            [
                PaneInput::Keys(keys(&["Enter"])),
                PaneInput::Keys(keys(&["Enter"]))
            ],
            "MAX_PROMPT_RESENDS presses"
        );
        assert_eq!(calls(&fake, "agent.prompt").len(), 1, "pasted once");
        let error = store.get_task(t.id).unwrap().unwrap().error.unwrap();
        assert!(error.contains("did not take"), "{error}");
    }

    /// Codex's folder-trust dialog reads idle, and Enter on it would trust the
    /// folder: with the dialog on the screen nothing is pressed, however
    /// much of a paste the pane shows.
    #[tokio::test(start_paused = true)]
    async fn enter_is_not_pressed_on_codex_s_trust_dialog() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_of(&store, "codex").id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_pane_text(
            &pane,
            "[Pasted Content 4096 chars]\n\n  Do you trust the contents of this directory?\n  /srv/repo\n\n› 1. Yes, continue\n  2. No, quit\n\n  Press enter to continue\n",
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            fake.pane_input(&pane).is_empty(),
            "{:?}",
            fake.pane_input(&pane)
        );
    }

    /// A codex prompt that is nowhere on its screen is sent again, as any
    /// agent's is, and no Enter is pressed into an empty input.
    #[tokio::test(start_paused = true)]
    async fn a_codex_prompt_that_is_not_in_its_input_is_sent_again() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_of(&store, "codex").id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_pane_text(&pane, "  gpt-5 default\n\n› \n");
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        assert_eq!(
            calls(&fake, "agent.prompt").len(),
            1 + MAX_PROMPT_RESENDS as usize
        );
        assert!(fake.pane_input(&pane).is_empty(), "no Enter into nothing");
    }

    /// The same for an agent that took its trust answer: a prompt lost
    /// anyway (a redraw longer than `settle`) is sent again.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_lost_after_a_trust_answer_is_sent_again() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        fake.set_trust_redraw(Duration::from_millis(250));
        let settings = settings_with_settle(Duration::from_millis(100));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h
            .dispatch(repo_task(&store, "claude", Some("~/src/app")).id)
            .await
            .unwrap();
        fake.answer_trust_by_hand(t.pane_id.as_deref().unwrap());
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        assert!(calls(&fake, "agent.prompt").len() >= 2);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// herdr can read Claude's folder-trust dialog idle and ready (late,
    /// or not at all). A prompt typed then goes into the dialog and is
    /// lost, so the first prompt waits while the dialog shows: pastor
    /// answers it for a trusted repo, and the prompt goes in once, after.
    #[tokio::test(start_paused = true)]
    async fn a_claude_prompt_waits_for_a_trust_dialog_read_idle() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        fake.set_trust_idle(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(calls(&fake, "agent.prompt").is_empty());
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert!(store.answered(t.id, "trust").unwrap());
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// The same dialog in a folder pastor does not trust waits for a
    /// person; once they answer it at the pane, the prompt goes in once.
    #[tokio::test(start_paused = true)]
    async fn a_claude_prompt_held_at_a_trust_dialog_goes_in_once_answered() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        fake.set_trust_idle(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h
            .dispatch(repo_task(&store, "claude", Some("~/src/app")).id)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        let pane = t.pane_id.clone().unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(calls(&fake, "agent.prompt").is_empty());
        assert!(fake.pane_input(&pane).is_empty(), "nothing pressed for it");

        fake.answer_trust_by_hand(&pane);
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// herdr read the agent ready before Claude drew its trust dialog, so
    /// the prompt went into the dialog. Once herdr reads it blocked with
    /// the dialog on screen, that is not work: the prompt is pending again
    /// and the task blocked, and the prompt goes in again once the dialog
    /// is answered.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_typed_into_a_trust_dialog_is_held_and_sent_again() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        let pane = t.pane_id.clone().unwrap();
        fake.set_pane_text(&pane, crate::herdr::fake::CLAUDE_TRUST_SCREEN);
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("the prompt held again", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Blocked && t.prompt_pending
        })
        .await;
        assert!(!store.get_task(t.id).unwrap().unwrap().activity_seen);

        // A person answers the dialog; the agent is idle at an empty input.
        fake.set_pane_text(&pane, "fake output");
        fake.ignore_prompts(false);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 2);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// A prompt held after it went into a dialog keeps its resend count
    /// through a moment of `working` while the agent redraws, so the socket
    /// send after the answer is marked as a resend and Claude does not drop
    /// it as the same text from the same sender.
    #[tokio::test(start_paused = true)]
    async fn a_held_prompt_keeps_its_attempt_through_a_redraw() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(fake.inbox_sends().len(), 1);
        let pane = t.pane_id.clone().unwrap();
        fake.set_pane_text(&pane, crate::herdr::fake::CLAUDE_TRUST_SCREEN);
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("the prompt held again", || {
            store.get_task(t.id).unwrap().unwrap().prompt_pending
        })
        .await;
        fake.set_pane_text(&pane, "fake output");
        fake.set_status(&pane, AgentStatus::Working);
        tokio::time::sleep(Duration::from_millis(50)).await;
        fake.ignore_prompts(false);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("sent again", || fake.inbox_sends().len() == 2).await;
        let sends = fake.inbox_sends();
        assert!(sends[1].text.contains("attempt 2"), "{:?}", sends[1].text);
    }

    /// The prompt's own Enter answered a dialog herdr read blocked for a
    /// moment: the agent sits idle with the prompt nowhere on its screen.
    /// That blocked spell was no work on the prompt, so the task is not
    /// done: the prompt is sent again.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_lost_in_a_dialog_it_answered_is_sent_again() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        fake.ignore_prompts(false);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 2);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// Every attempt lost the same way, blocked for a moment and idle with
    /// the prompt nowhere on screen: after the resends the task is
    /// `blocked` for a person, and stays so while the agent sits idle,
    /// rather than ending at the next settle.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_lost_in_a_dialog_every_time_ends_blocked_and_stays() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let did_not_take = || {
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .error
                .is_some_and(|e| e.contains("did not take"))
        };
        for sent in 1..=1 + MAX_PROMPT_RESENDS as usize {
            wait_for("the prompt sent", || {
                calls(&fake, "agent.prompt").len() == sent
            })
            .await;
            fake.set_status(&pane, AgentStatus::Blocked);
            wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
            fake.set_status(&pane, AgentStatus::Idle);
        }
        wait_for("blocked for a person", did_not_take).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(did_not_take());
        assert_eq!(
            calls(&fake, "agent.prompt").len(),
            1 + MAX_PROMPT_RESENDS as usize
        );
    }

    /// A blocked spell before any work, with the prompt on the screen
    /// after it, did reach the agent: its idle after that ends the task as
    /// before, with no prompt sent again.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_on_screen_after_a_blocked_spell_was_taken() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        // The prompt alone, no message after it: the prompt is what shows.
        let shown = format!("> {}\n", crate::task::prompt_to_send(&t));
        fake.set_pane_text(&pane, &shown);
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("ended", || {
            matches!(state_of(&store, t.id), TaskState::Done | TaskState::Failed)
        })
        .await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
    }

    /// The same for the actor's own answer to a trusted repo's dialog.
    #[tokio::test(start_paused = true)]
    async fn auto_trust_delivers_the_prompt_once_after_the_agent_redraws() {
        let (fake, settings) = redrawing_after_trust();
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// `--trust` answers only the startup prompt: a task that is starting,
    /// running, or blocked on something else is refused, and nothing is
    /// sent, saved or claimed, so saved trust can still answer it later.
    #[tokio::test(start_paused = true)]
    async fn send_trust_refuses_a_task_not_at_its_startup_prompt() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let running = h
            .dispatch(repo_task(&store, "claude", Some("/r")).id)
            .await
            .unwrap();
        assert_eq!(running.state, TaskState::Running);
        let blocked_later = h
            .dispatch(repo_task(&store, "claude", Some("/r")).id)
            .await
            .unwrap();
        fake.set_status(
            blocked_later.pane_id.as_deref().unwrap(),
            AgentStatus::Blocked,
        );
        wait_for("blocked", || {
            state_of(&store, blocked_later.id) == TaskState::Blocked
        })
        .await;
        for t in [&running, &blocked_later] {
            let err = h.send(t.id, trust()).await.unwrap_err();
            assert_eq!(
                err.downcast_ref::<SendRefused>().map(|r| r.code),
                Some("not_at_trust_prompt"),
                "{err:#}"
            );
            // Not claimed: the first claim still wins.
            assert!(store.claim_trust_sent(t.id, &["Enter".into()]).unwrap());
        }
        assert!(calls(&fake, "pane.send_keys").is_empty());
        assert!(store.trusted_repos().unwrap().is_empty());
        assert!(input_events(&mut events).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn send_trust_needs_trust_keys() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let codex = h
            .dispatch(repo_task(&store, "codex", Some("/r")).id)
            .await
            .unwrap();
        let err = h.send(codex.id, trust()).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<SendRefused>().map(|r| r.code),
            Some("no_trust_keys"),
            "{err:#}"
        );
        assert!(calls(&fake, "pane.send_keys").is_empty());
    }

    /// A task with no repo starts in the folder dispatch picked for it
    /// (`~/pastor-tasks`, made absolute), and that folder is what `--trust`
    /// saves: the next task with no repo on the machine, blocked on the same
    /// dialog, is answered by the actor and runs, with no one at the pane.
    /// Another machine's trust does not count, nor does another folder.
    #[tokio::test(start_paused = true)]
    async fn trust_for_a_task_with_no_repo_is_saved_for_its_folder() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let bare = h
            .dispatch(repo_task(&store, "claude", None).id)
            .await
            .unwrap();
        assert_eq!(bare.state, TaskState::Blocked);
        let dir = bare
            .spec
            .cwd
            .clone()
            .expect("a task with no repo records its folder");
        assert!(dir.ends_with("/pastor-tasks"), "{dir}");
        assert!(store.trusted_repos().unwrap().is_empty());
        h.send(bare.id, trust()).await.unwrap();
        // `Down`, then `Enter` once the pane confirms it landed.
        assert_eq!(calls(&fake, "pane.send_keys").len(), 2);
        assert!(store.is_trusted("m", &dir).unwrap());
        assert_eq!(store.trusted_repos().unwrap().len(), 1);
        trusted_events(&mut events);

        // The next one needs no one.
        let next = h
            .dispatch(repo_task(&store, "claude", None).id)
            .await
            .unwrap();
        assert_eq!(next.spec.cwd.as_deref(), Some(dir.as_str()));
        wait_for("the second task answered and running", || {
            state_of(&store, next.id) == TaskState::Running
        })
        .await;
        assert_eq!(trusted_events(&mut events).len(), 1);
        // The two sends of `--trust`, then the actor's one for the second.
        assert_eq!(calls(&fake, "pane.send_keys").len(), 3);

        // A task from before the folder was recorded has nothing to save,
        // and one in a folder nobody trusted is left to a person.
        let mut old = repo_task(&store, "claude", None);
        old.spec.cwd = None;
        assert_eq!(old.spec.trust_dir(), None);
        store.untrust("m", &dir).unwrap();
        let again = h
            .dispatch(repo_task(&store, "claude", None).id)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(state_of(&store, again.id), TaskState::Blocked);
    }

    fn answered_events(events: &mut broadcast::Receiver<PastorEvent>) -> Vec<PastorEvent> {
        let mut out = vec![];
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "task.answered" {
                out.push(ev);
            }
        }
        out
    }

    /// The kinds of every event so far, in order, with `task.*` only.
    fn task_event_kinds(events: &mut broadcast::Receiver<PastorEvent>) -> Vec<String> {
        let mut out = vec![];
        while let Ok(ev) = events.try_recv() {
            if ev.kind.starts_with("task.") {
                out.push(ev.kind);
            }
        }
        out
    }

    async fn connected_with(
        fake: &FakeHerdr,
        store: &Arc<Store>,
        settings: MachineSettings,
    ) -> (MachineHandle, broadcast::Receiver<PastorEvent>) {
        let (h, events) = spawn_with_settings(fake, store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        (h, events)
    }

    const AUTO_SCREEN: &str = "Make auto mode your default permission mode?\n\n ❯ Yes, set auto mode as my default permission mode\n   No, keep default\n\n Enter to confirm · Esc to cancel\n";
    const RENDERER_SCREEN: &str = "  Try the new fullscreen renderer?\n\n  ❯ 1. Yes, try it\n    2. Not now\n\n  Enter to confirm · Esc to cancel\n";
    const DOWN_ENTER: [&str; 2] = ["Down", "Enter"];

    fn keys(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    /// Trust, then the auto-mode nudge: each answered once, in order, and
    /// the prompt goes in after the second.
    #[tokio::test(start_paused = true)]
    async fn a_chain_of_startup_dialogs_is_answered_one_per_settle() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (
                crate::herdr::fake::CLAUDE_TRUST_SCREEN.into(),
                keys(&DOWN_ENTER),
            ),
            (AUTO_SCREEN.into(), keys(&DOWN_ENTER)),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the task to run", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [
                PaneInput::Keys(keys(&DOWN_ENTER)),
                PaneInput::Keys(keys(&DOWN_ENTER)),
            ]
        );
        let mut evs = vec![];
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "task.trusted" || ev.kind == "task.answered" {
                evs.push(ev);
            }
        }
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["task.trusted", "task.answered"]);
        assert_eq!(
            evs[1].detail,
            Some(serde_json::json!({"dialog": "auto-mode-default", "keys": ["Down", "Enter"]}))
        );
        assert!(store.answered(t.id, "auto-mode-default").unwrap());
        assert!(store.answered(t.id, "trust").unwrap());
    }

    /// The same chain read idle and ready, as herdr can read Claude's
    /// dialogs before it notices them: the prompt waits for the built-in
    /// auto-mode dialog too, not only the trust one, and goes in once after.
    #[tokio::test(start_paused = true)]
    async fn a_claude_prompt_waits_for_its_builtin_dialogs_read_idle() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (
                crate::herdr::fake::CLAUDE_TRUST_SCREEN.into(),
                keys(&DOWN_ENTER),
            ),
            (AUTO_SCREEN.into(), keys(&DOWN_ENTER)),
        ]);
        fake.set_trust_idle(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the agent working on its prompt", || agent_working(&fake)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert!(store.answered(t.id, "auto-mode-default").unwrap());
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// Claude's autocompact offer, as it draws it under a finished turn.
    const AUTOCOMPACT_SCREEN: &str = "✻ Sautéed for 14m 37s\n\n─────────\n We recommend compacting earlier\n\n   Want to autocompact at 400k tokens instead?\n\n   ❯ Autocompact at ‹ 400k › tokens\n     Keep 600k tokens\n\n   ↑/↓ choose · ←/→ change size · enter confirm · esc not now\n";

    /// At a task's start herdr reads the offer idle and ready, so a prompt
    /// typed then is lost in it: the prompt waits, the offer is answered
    /// with "Keep", and the prompt goes in once it is gone.
    #[tokio::test(start_paused = true)]
    async fn the_autocompact_offer_at_a_start_is_kept_and_the_prompt_goes_in() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(AUTOCOMPACT_SCREEN.into(), keys(&DOWN_ENTER))]);
        fake.set_trust_idle(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        let pane = t.pane_id.clone().unwrap();
        wait_for("the task to run", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        assert_eq!(fake.pane_input(&pane), [PaneInput::Keys(keys(&DOWN_ENTER))]);
        assert_eq!(calls(&fake, "agent.prompt").len(), 1);
        assert!(store.answered(t.id, "autocompact").unwrap());
        let mut answered = vec![];
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "task.answered" {
                answered.push(ev.detail);
            }
        }
        assert_eq!(
            answered,
            [Some(
                serde_json::json!({"dialog": "autocompact", "keys": ["Down", "Enter"]})
            )]
        );
    }

    /// Under a finished turn the offer replaces the input box. The agent
    /// is idle, so the task would close as finished with no summary while
    /// Claude waits for an answer; pastor keeps the setting instead, and
    /// the task ends once the pane is back to its input box.
    #[tokio::test(start_paused = true)]
    async fn the_autocompact_offer_under_an_idle_agent_is_answered_not_closed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(&pane, AUTOCOMPACT_SCREEN);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("the offer answered", || {
            fake.pane_input(&pane) == [PaneInput::Keys(keys(&DOWN_ENTER))]
        })
        .await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "not closed under the offer"
        );
        // Claude is back at its input box: the turn is over.
        fake.set_pane_text(&pane, "✻ Sautéed for 14m 37s\n\n❯ \n");
        tokio::time::sleep(settle * 6).await;
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert_eq!(
            fake.pane_input(&pane),
            [PaneInput::Keys(keys(&DOWN_ENTER))],
            "pressed once"
        );
    }

    /// The offer is still up after its answer (Claude redrew slowly, or a
    /// key was lost), or comes up a second time, or its "Keep" option is not
    /// there: nothing is pressed twice, and the task waits for a person
    /// blocked, not closed under it.
    #[tokio::test(start_paused = true)]
    async fn the_autocompact_offer_that_stays_up_blocks_the_task_not_closes_it() {
        for (screen, presses) in [
            (AUTOCOMPACT_SCREEN.to_string(), 1),
            (AUTOCOMPACT_SCREEN.replace("Keep 600k", "Stay at 600k"), 0),
        ] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let settle = Duration::from_millis(100);
            let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            let t = h.dispatch(new_task(&store).id).await.unwrap();
            let pane = t.pane_id.clone().unwrap();
            fake.set_status(&pane, AgentStatus::Working);
            wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
            fake.set_pane_text(&pane, &screen);
            fake.set_status(&pane, AgentStatus::Idle);
            wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
            tokio::time::sleep(settle * 6).await;
            assert_eq!(state_of(&store, t.id), TaskState::Blocked, "{screen}");
            assert_eq!(fake.pane_input(&pane).len(), presses, "{screen}");
            let error = store.get_task(t.id).unwrap().unwrap().error.unwrap();
            assert!(error.contains("autocompact"), "{error}");
        }
    }

    /// A resumed task that comes back blocked on a startup dialog pastor
    /// knows has it answered before `resume` replies, as a dispatched one
    /// does, not only at the next reconcile.
    #[tokio::test(start_paused = true)]
    async fn a_resumed_task_at_a_dialog_has_it_answered() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(RENDERER_SCREEN.into(), keys(&DOWN_ENTER))]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let mut t = new_task(&store);
        t.state = TaskState::Paused;
        t.machine = Some("m".into());
        t.spec.session_id = crate::task::new_session_id();
        store.update_task(&mut t).unwrap();
        let t = h.resume(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [PaneInput::Keys(keys(&DOWN_ENTER))]
        );
        assert!(store.answered(t.id, "fullscreen").unwrap());
    }

    /// A dialog after trust whose row the user removed is left for a
    /// person; trust's keys are not pressed again.
    #[tokio::test(start_paused = true)]
    async fn a_removed_dialog_is_left_for_a_person() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (
                crate::herdr::fake::CLAUDE_TRUST_SCREEN.into(),
                keys(&DOWN_ENTER),
            ),
            (RENDERER_SCREEN.into(), keys(&DOWN_ENTER)),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let mut settings = settings();
        settings.agents = toml::from_str(
            "[claude]\n[[claude.dialogs]]\nname = \"fullscreen\"\nmarker = \"x\"\npick = \"\"\n",
        )
        .unwrap();
        let (h, mut events) = connected_with(&fake, &store, settings).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [PaneInput::Keys(keys(&DOWN_ENTER))]
        );
        assert!(answered_events(&mut events).is_empty());
    }

    /// A pick whose option is not on screen presses nothing and claims
    /// nothing, so a later screen with the option is still answered.
    #[tokio::test(start_paused = true)]
    async fn a_pick_not_in_the_picker_is_not_pressed() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(
            "  Try the new fullscreen renderer?\n\n  ❯ 1. Yes, try it\n    2. Later\n\n  Enter to confirm · Esc to cancel\n".into(),
            keys(&DOWN_ENTER),
        )]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(calls(&fake, "pane.send_keys").is_empty());
        assert!(!store.answered(t.id, "fullscreen").unwrap());
    }

    /// An untrusted repo skips the trust row but not the rows after it: a
    /// task that starts on the renderer offer is answered.
    #[tokio::test(start_paused = true)]
    async fn rows_after_trust_do_not_need_saved_trust() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(RENDERER_SCREEN.into(), keys(&DOWN_ENTER))]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the task to run", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        let kinds = task_event_kinds(&mut events);
        assert!(!kinds.iter().any(|k| k == "task.trusted"), "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|k| *k == "task.answered").count(),
            1,
            "{kinds:?}"
        );
    }

    /// Restarted actor, same store: an answered dialog is not pressed again.
    #[tokio::test(start_paused = true)]
    async fn an_answered_dialog_is_not_pressed_again_after_a_restart() {
        let fake = FakeHerdr::new();
        // Keys that do not answer it: the agent stays blocked.
        fake.set_startup_dialogs(vec![(RENDERER_SCREEN.into(), keys(&["Enter"]))]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _e) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "pane.send_keys").len(), 1);
        // Its agent is still up, so the shutdown reports that; the actor is
        // gone either way.
        let _ = h.shutdown().await;
        let (_h2, _e2) = connected(&fake, &store).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            calls(&fake, "pane.send_keys").len(),
            1,
            "claimed in the store"
        );
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
    }

    fn pick(what: &str) -> SendInput {
        SendInput {
            pick: Some(what.into()),
            ..Default::default()
        }
    }

    /// `--pick` reads the picker and presses the arrows to the option, then
    /// Enter; it saves no trust.
    #[tokio::test(start_paused = true)]
    async fn send_pick_arrows_to_the_option_and_confirms() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(BYPASS_SCREEN.into(), keys(&DOWN_ENTER))]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        h.send(t.id, pick("yes, i accept")).await.unwrap();
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [PaneInput::Keys(keys(&DOWN_ENTER))]
        );
        let evs = input_events(&mut events);
        assert_eq!(
            evs[0].detail,
            Some(serde_json::json!({"keys": ["Down", "Enter"], "pick": "yes, i accept"}))
        );
        assert!(
            !store.is_trusted("m", "/r").unwrap(),
            "--pick never saves trust"
        );
    }

    const BYPASS_SCREEN: &str = "WARNING: Claude Code running in Bypass Permissions mode\n\n\u{276f} 1. No, exit\n  2. Yes, I accept\n\nEnter to confirm \u{b7} Esc to cancel\n";

    /// On a dialog pastor does not answer itself (the bypass warning).
    #[tokio::test(start_paused = true)]
    async fn send_pick_refuses_without_a_picker_or_the_option() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(BYPASS_SCREEN.into(), keys(&["Enter"]))]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _e) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        let err = h.send(t.id, pick("upgrade")).await.unwrap_err();
        let r = err.downcast_ref::<SendRefused>().unwrap();
        assert_eq!(r.code, "no_such_option");
        assert!(
            r.message.contains("1. No, exit, 2. Yes, I accept"),
            "{}",
            r.message
        );
        // Answered by hand: the pane shows no picker now.
        fake.pane_send_keys(t.pane_id.as_deref().unwrap(), &keys(&["Enter"]))
            .await
            .unwrap();
        let err = h.send(t.id, pick("Not now")).await.unwrap_err();
        assert_eq!(err.downcast_ref::<SendRefused>().unwrap().code, "no_picker");
        assert_eq!(
            calls(&fake, "pane.send_keys").len(),
            1,
            "only the hand answer"
        );
    }

    /// The trust dialog as 2.1.295 draws it: options without numbers.
    const TRUST_SCREEN: &str = "Quick safety check: Is this a project you created or one you trust?\n\n \u{276f} No, exit\n   Yes, I trust this folder\n\n Enter to confirm \u{b7} Esc to cancel\n";

    /// A trust row with no marker matches any screen; once it is answered
    /// the rows after it are still tried, so the chain goes on.
    #[tokio::test(start_paused = true)]
    async fn an_answered_row_does_not_stop_the_rows_after_it() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (TRUST_SCREEN.into(), keys(&DOWN_ENTER)),
            (RENDERER_SCREEN.into(), keys(&DOWN_ENTER)),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let mut settings = settings();
        settings.agents = toml::from_str("[claude]\ntrust_marker = \"\"\n").unwrap();
        let (h, _e) = connected_with(&fake, &store, settings).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the task to run", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        assert!(store.answered(t.id, "trust").unwrap());
        assert!(store.answered(t.id, "fullscreen").unwrap());
    }

    /// An untrusted repo's trust dialog, answered by a person, can leave
    /// its text above the next dialog in the pane read; the trust row does
    /// not apply, and the renderer row after it is still answered.
    #[tokio::test(start_paused = true)]
    async fn a_lingering_trust_dialog_does_not_hide_the_next_one() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (TRUST_SCREEN.into(), keys(&DOWN_ENTER)),
            (
                format!("{TRUST_SCREEN}\n{RENDERER_SCREEN}"),
                keys(&DOWN_ENTER),
            ),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _e) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Blocked,
            "untrusted: left alone"
        );
        assert!(calls(&fake, "pane.send_keys").is_empty());
        // A person answers trust at the pane.
        fake.pane_send_keys(t.pane_id.as_deref().unwrap(), &keys(&DOWN_ENTER))
            .await
            .unwrap();
        wait_for("the task to run", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        assert!(!store.answered(t.id, "trust").unwrap());
        assert!(store.answered(t.id, "fullscreen").unwrap());
    }

    /// herdr reports no change when one dialog replaces another, so the
    /// next dialog is answered after the settle window, not at the next
    /// reconcile, which is far off here.
    #[tokio::test(start_paused = true)]
    async fn the_next_dialog_is_answered_after_the_settle_window() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (
                crate::herdr::fake::CLAUDE_TRUST_SCREEN.into(),
                keys(&DOWN_ENTER),
            ),
            (AUTO_SCREEN.into(), keys(&DOWN_ENTER)),
            (RENDERER_SCREEN.into(), keys(&DOWN_ENTER)),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let settings = MachineSettings {
            reconcile_every: Duration::from_secs(60),
            poll_every: Duration::from_secs(60),
            ..settings()
        };
        let (h, _e) = connected_with(&fake, &store, settings).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert_eq!(calls(&fake, "pane.send_keys").len(), 3);
    }

    /// Two tasks whose holds come due in the same settle tick, each on a
    /// chain, while Claude takes a moment to replace one dialog with the
    /// next. Answering the first task's next dialog must not drop the hold
    /// the second task just got for its own, or that task's third dialog
    /// waits for a reconcile. Dispatched a redraw apart so their holds are
    /// never in lockstep.
    #[tokio::test(start_paused = true)]
    async fn two_tasks_due_in_one_settle_tick_each_get_their_next_dialog() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (
                crate::herdr::fake::CLAUDE_TRUST_SCREEN.into(),
                keys(&DOWN_ENTER),
            ),
            (AUTO_SCREEN.into(), keys(&DOWN_ENTER)),
            (RENDERER_SCREEN.into(), keys(&DOWN_ENTER)),
        ]);
        fake.set_trust_redraw(Duration::from_millis(50));
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let settings = MachineSettings {
            reconcile_every: Duration::from_secs(60),
            poll_every: Duration::from_secs(60),
            ..settings()
        };
        let (h, _e) = connected_with(&fake, &store, settings).await;
        let t1 = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        let t2 = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(state_of(&store, t1.id), TaskState::Running);
        assert_eq!(state_of(&store, t2.id), TaskState::Running);
        assert_eq!(calls(&fake, "pane.send_keys").len(), 6);
    }

    /// A dialog is answered once per settle window, as the manual says:
    /// a status event or reconcile inside the window reads nothing, and
    /// the hold's own tick finds the next dialog.
    #[tokio::test(start_paused = true)]
    async fn a_task_in_its_settle_window_is_not_read_again() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (
                crate::herdr::fake::CLAUDE_TRUST_SCREEN.into(),
                keys(&DOWN_ENTER),
            ),
            (AUTO_SCREEN.into(), keys(&DOWN_ENTER)),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let settings = MachineSettings {
            reconcile_every: Duration::from_secs(60),
            poll_every: Duration::from_secs(60),
            ..settings()
        };
        let (h, _e) = connected_with(&fake, &store, settings).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        assert_eq!(calls(&fake, "pane.send_keys").len(), 1);
        // A reconcile right after the answer (`auto_trust` runs the pass).
        h.auto_trust().await.unwrap();
        assert_eq!(
            calls(&fake, "pane.send_keys").len(),
            1,
            "the next dialog waits for the settle window"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert_eq!(calls(&fake, "pane.send_keys").len(), 2);
    }

    /// A person answers a dialog pastor leaves alone (the bypass warning)
    /// with `--pick`; the dialog after it is pastor's to answer, and it is,
    /// after the settle window, with no reconcile in between.
    #[tokio::test(start_paused = true)]
    async fn a_dialog_after_a_person_s_pick_is_answered_after_the_settle_window() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (BYPASS_SCREEN.into(), keys(&DOWN_ENTER)),
            (RENDERER_SCREEN.into(), keys(&DOWN_ENTER)),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settings = MachineSettings {
            reconcile_every: Duration::from_secs(60),
            poll_every: Duration::from_secs(60),
            ..settings()
        };
        let (h, _e) = connected_with(&fake, &store, settings).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        h.send(t.id, pick("Yes, I accept")).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert_eq!(calls(&fake, "pane.send_keys").len(), 2);
    }

    fn trusted_events(events: &mut broadcast::Receiver<PastorEvent>) -> Vec<PastorEvent> {
        let mut out = vec![];
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "task.trusted" {
                out.push(ev);
            }
        }
        out
    }

    /// A worktree task of a trusted repo blocks on the trust prompt of its
    /// new folder; the actor answers it once and the task runs.
    #[tokio::test(start_paused = true)]
    async fn a_task_of_a_trusted_repo_is_answered_once_and_runs() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the task to run", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        // Some reconciles later, still once.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [PaneInput::Keys(vec!["Down".into(), "Enter".into()])]
        );
        let evs = trusted_events(&mut events);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].task_id, Some(t.id));
    }

    /// A trust send that fails (here it times out) is not a send: a later
    /// reconcile tries again, and the task runs once one gets through.
    #[tokio::test(start_paused = true)]
    async fn a_failed_trust_send_is_tried_again() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                request_timeout: Duration::from_secs(1),
                ..settings()
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        fake.hang_method("pane.send_keys");
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the task to run", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        assert_eq!(calls(&fake, "pane.send_keys").len(), 2);
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [PaneInput::Keys(vec!["Down".into(), "Enter".into()])]
        );
        assert_eq!(trusted_events(&mut events).len(), 1);
    }

    /// Saved trust reads the pane before it presses anything: a task of a
    /// trusted repo blocked on some other dialog, one the same keys would
    /// accept (Claude's bypass-permissions warning opens on "No, exit" too),
    /// is left blocked for a human.
    #[tokio::test(start_paused = true)]
    async fn saved_trust_leaves_a_dialog_that_is_not_the_trust_prompt_alone() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        fake.set_trust_screen(
            "WARNING: Claude Code running in Bypass Permissions mode\n\u{276f} 1. No, exit\n  2. Yes, I accept\n",
        );
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!calls(&fake, "agent.read").is_empty(), "the pane was read");
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(calls(&fake, "pane.send_keys").is_empty());
        assert!(trusted_events(&mut events).is_empty());
        // Not claimed: once the real dialog shows, it is answered.
        assert!(!store.trust_sent(t.id).unwrap());
    }

    /// The pane read is scrollback: a trust prompt still in it above another
    /// dialog is not the prompt on screen, and gets no keys.
    #[tokio::test(start_paused = true)]
    async fn saved_trust_ignores_a_trust_prompt_left_in_scrollback() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        fake.set_trust_screen(
            "Quick safety check: Is this a project you trust?\n\n\u{276f} 1. No, exit\n  2. Yes, I trust this folder\n\n\
             WARNING: Claude Code running in Bypass Permissions mode\n\u{276f} 1. No, exit\n  2. Yes, I accept\n",
        );
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!calls(&fake, "agent.read").is_empty(), "the pane was read");
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(calls(&fake, "pane.send_keys").is_empty());
        assert!(trusted_events(&mut events).is_empty());
        assert!(!store.trust_sent(t.id).unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_of_an_untrusted_repo_stays_blocked() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        // Trusted on another machine only.
        store.trust_repo("other", "/r").unwrap();
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(calls(&fake, "pane.send_keys").is_empty());
        assert!(trusted_events(&mut events).is_empty());
    }

    /// Keys that do not answer the prompt leave the task blocked for a
    /// human; the actor does not try again.
    #[tokio::test(start_paused = true)]
    async fn a_task_still_blocked_after_the_trust_keys_is_left_to_a_human() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert_eq!(calls(&fake, "pane.send_keys").len(), 1);
        assert_eq!(trusted_events(&mut events).len(), 1);
        // A new actor, as after a restart, does not send them again either.
        assert_eq!(h.shutdown().await, ShutdownOutcome::Finished);
        let (h2, _events) = connected(&fake, &store).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "pane.send_keys").len(), 1);
        drop(h2);
    }

    #[tokio::test(start_paused = true)]
    async fn send_refuses_a_task_that_is_not_live() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        h.close(t.id, false).await.unwrap();
        let err = h
            .send(
                t.id,
                SendInput {
                    keys: vec!["Enter".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<SendRefused>().map(|r| r.code),
            Some("task_not_live"),
            "{err:#}"
        );
        assert!(calls(&fake, "pane.send_keys").is_empty());
        assert!(input_events(&mut events).is_empty());
    }

    /// A task marked done with its work unfinished can be told to finish:
    /// the input goes into its still-open pane and the task runs again. The
    /// baseline stays at the idle it was done at, so only the agent's next
    /// turn marks it done again.
    #[tokio::test(start_paused = true)]
    async fn send_to_a_done_task_types_into_its_pane_and_runs_it_again() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = run_to_done(&h, &fake, &store, new_task(&store)).await;
        let pane = t.pane_id.clone().unwrap();
        while events.try_recv().is_ok() {}
        let sent = h
            .send(
                t.id,
                SendInput {
                    text: Some("commit and push".into()),
                    enter: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(sent.state, TaskState::Running);
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Running);
        assert_eq!(row.finished_at, None);
        assert_eq!(
            fake.pane_input(&pane),
            [
                crate::herdr::fake::PaneInput::Text(format!(
                    "commit and push {}",
                    crate::task::SUMMARY_ASK
                )),
                crate::herdr::fake::PaneInput::Keys(vec!["Enter".into()]),
            ]
        );
        let mut kinds = vec![];
        while let Ok(ev) = events.try_recv() {
            kinds.push(ev.kind);
        }
        assert!(kinds.contains(&"task.running".to_string()), "{kinds:?}");
        // Still idle at the old completion: running, not done again.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        // Its new turn finishes it.
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || {
            store.get_task(t.id).unwrap().unwrap().activity_seen
        })
        .await;
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done again", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// A done row with no pane has nothing to type into.
    #[tokio::test(start_paused = true)]
    async fn send_refuses_a_done_task_with_no_pane() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let mut t = new_task(&store);
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        store.update_task(&mut t).unwrap();
        let err = h
            .send(
                t.id,
                SendInput {
                    text: Some("commit".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<SendRefused>().map(|r| r.code),
            Some("task_not_live"),
            "{err:#}"
        );
        assert_eq!(state_of(&store, t.id), TaskState::Done);
    }

    fn auto_close_settings() -> MachineSettings {
        MachineSettings {
            reconcile_every: Duration::from_millis(100),
            close_done_after: Some(Duration::from_millis(150)),
            ..settings()
        }
    }

    /// Dispatch `task` and let its agent finish: it goes idle after working,
    /// so the settle window marks it done.
    async fn run_to_done(h: &MachineHandle, fake: &FakeHerdr, store: &Store, task: Task) -> Task {
        let t = h.dispatch(task.id).await.unwrap();
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        wait_for("done", || state_of(store, t.id) == TaskState::Done).await;
        store.get_task(t.id).unwrap().unwrap()
    }

    /// Moves a task's finish an hour back, well past `close_done_after`,
    /// retrying when the actor wrote the row in between. The grace is calendar
    /// time, which a paused clock does not move, so no test can wait it out.
    fn finished_an_hour_ago(store: &Store, id: i64) -> Task {
        loop {
            let mut t = store.get_task(id).unwrap().unwrap();
            t.finished_at = Some(Utc::now() - chrono::Duration::hours(1));
            match store.update_task(&mut t) {
                Ok(()) => return t,
                Err(err) if err.downcast_ref::<crate::store::Conflict>().is_some() => {}
                Err(err) => panic!("{err:#}"),
            }
        }
    }

    /// Waits for a task to be done, puts its grace behind it and waits for
    /// auto-close to take it.
    async fn auto_closed(store: &Store, id: i64) {
        wait_for("done", || state_of(store, id) == TaskState::Done).await;
        finished_an_hour_ago(store, id);
        wait_for("closed", || state_of(store, id) == TaskState::Closed).await;
    }

    fn count(
        events: &mut broadcast::Receiver<PastorEvent>,
        kind: &str,
        id: i64,
    ) -> Vec<PastorEvent> {
        let mut found = vec![];
        while let Ok(ev) = events.try_recv() {
            if ev.kind == kind && ev.task_id == Some(id) {
                found.push(ev);
            }
        }
        found
    }

    /// A grace shorter than `reconcile_every` is not stretched to it: auto-close
    /// runs on its own tick, so the task closes long before the next reconcile.
    #[tokio::test(start_paused = true)]
    async fn a_short_grace_closes_before_the_next_reconcile() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settings = MachineSettings {
            reconcile_every: Duration::from_secs(30),
            ..auto_close_settings()
        };
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = run_to_done(&h, &fake, &store, new_task(&store)).await;
        auto_closed(&store, t.id).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_done_task_is_closed_after_close_done_after() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        // Half an hour of grace: no stall of the test's own can use it up.
        let settings = MachineSettings {
            close_done_after: Some(Duration::from_secs(1800)),
            ..auto_close_settings()
        };
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = run_to_done(&h, &fake, &store, new_task(&store)).await;
        // Some auto-close checks later, the finish is still a moment ago.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert!(
            calls(&fake, "pane.close").is_empty(),
            "the pane stays open for the grace period"
        );
        let t = finished_an_hour_ago(&store, t.id);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        let closed = store.get_task(t.id).unwrap().unwrap();
        assert!(
            (closed.finished_at.unwrap() - t.finished_at.unwrap()).num_milliseconds() == 0,
            "closing keeps the finish time"
        );
        assert!(closed.error.is_none(), "{:?}", closed.error);
        assert_eq!(
            calls(&fake, "pane.close"),
            vec![serde_json::json!({"pane_id": t.pane_id.clone().unwrap()})]
        );
        assert!(calls(&fake, "worktree.remove").is_empty());
        assert!(fake.agents().is_empty());
        wait_for("live drops", || h.snapshot().live == 0).await;
    }

    fn close_failed_settings() -> MachineSettings {
        MachineSettings {
            reconcile_every: Duration::from_millis(100),
            close_failed_after: Some(Duration::from_millis(150)),
            ..settings()
        }
    }

    /// A task in `state` on machine `m` holding the pane of an idle agent
    /// named after it, finished now, with no completion since its prompt.
    async fn stopped_task(fake: &FakeHerdr, store: &Store, state: TaskState) -> Task {
        let mut t = new_task(store);
        let pane = start_agent(fake, &Task::agent_name_for(t.id)).await;
        t.state = state;
        t.machine = Some("m".into());
        t.pane_id = Some(pane);
        t.agent_name = Some(Task::agent_name_for(t.id));
        t.started_at = Some(Utc::now());
        t.finished_at = Some(Utc::now());
        // A baseline no idle reaches: a stale task's agent has not finished.
        t.last_completion_seq = Some(1_000);
        store.update_task(&mut t).unwrap();
        t
    }

    fn pane_open(fake: &FakeHerdr, pane: &str) -> bool {
        fake.agents().iter().any(|a| a.pane_id == pane)
    }

    /// A failed task's stopped agent holds a pane and a slot: auto-close
    /// takes the pane after `close_failed_after`. The row stays failed, so
    /// `task retry` still takes it, and records no pane any more.
    #[tokio::test]
    async fn a_failed_tasks_pane_closes_after_close_failed_after() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let t = stopped_task(&fake, &store, TaskState::Failed).await;
        let pane = t.pane_id.clone().unwrap();
        let (h, _events) = spawn_with_settings(&fake, &store, close_failed_settings());
        wait_for("pane closed", || !pane_open(&fake, &pane)).await;
        assert_eq!(
            calls(&fake, "pane.close"),
            vec![serde_json::json!({"pane_id": pane})]
        );
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        assert_eq!(row.pane_id, None);
        assert_eq!(row.error, t.error);
        assert!(calls(&fake, "worktree.remove").is_empty());
        wait_for("live drops", || h.snapshot().live == 0).await;
        assert!(h.snapshot().orphans.is_empty());
        store.insert_retry(t.id).expect("still retryable");
    }

    /// A task that must end with a summary and goes idle without one, its
    /// worktree holding uncommitted files, may have been read idle too
    /// early: it is blocked, not failed, its pane kept past
    /// `close_failed_after`, and the agent can still finish it with `task
    /// done` and a summary. With a clean worktree the same stop fails.
    #[tokio::test(start_paused = true)]
    async fn a_required_summary_with_uncommitted_work_blocks_instead_of_failing() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(100);
        let settings = MachineSettings {
            close_failed_after: Some(Duration::from_millis(150)),
            ..settings_with_settle(settle)
        };
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let required = |store: &Store| {
            let mut t = worktree_task(store);
            t.spec.summary = crate::task::SummaryMode::Require;
            store.update_task(&mut t).unwrap();
            t
        };
        let t = h.dispatch(required(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let path = t.spec.checkout.clone().expect("a checkout").path;
        fake.set_dirty_checkout(&path);
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(&pane, "● Tests pass; committing next.\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert!(
            row.error
                .as_deref()
                .is_some_and(|e| e.starts_with(crate::task::STOPPED_WITHOUT_SUMMARY)),
            "{row:?}"
        );
        tokio::time::sleep(settle * 6).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(fake.agents().iter().any(|a| a.pane_id == pane), "pane kept");

        // The agent comes back, commits and ends the task itself.
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_clean_checkout(&path);
        let done = h
            .end_by(t.id, Some("done\ncommitted".into()), EndBy::Agent)
            .await
            .unwrap();
        assert_eq!(done.state, TaskState::Done);
        assert_eq!(done.summary.unwrap().outcome, crate::task::Outcome::Done);

        // A clean worktree: the same stop fails, as without a worktree.
        let t = h.dispatch(required(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        fake.set_pane_text(&pane, "● All finished\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
    }

    /// A worktree task that asked for its worktree to go whatever it ends in
    /// (`remove_worktree`), in `place`.
    fn removing_task(store: &Store, place: Place) -> Task {
        let mut t = placed_task(store, place, true);
        t.spec.remove_worktree = Some(true);
        store.update_task(&mut t).unwrap();
        t
    }

    /// A machine actor with `close_failed_after` short, connected.
    async fn connected_closing_failed(
        fake: &FakeHerdr,
        store: &Arc<Store>,
    ) -> (MachineHandle, broadcast::Receiver<PastorEvent>) {
        let (h, events) = spawn_with_settings(fake, store, close_failed_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        (h, events)
    }

    /// Dispatch `t`, then let its agent exit: the task fails.
    async fn run_to_failed(h: &MachineHandle, fake: &FakeHerdr, store: &Store, t: &Task) -> Task {
        let t = h.dispatch(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
        fake.exit_pane(t.pane_id.as_deref().unwrap());
        wait_for("failed", || state_of(store, t.id) == TaskState::Failed).await;
        store.get_task(t.id).unwrap().unwrap()
    }

    /// The pane of task `id` is closed (or its worktree removed with it).
    async fn pane_gone(store: &Store, id: i64) -> Task {
        wait_for("pane gone", || {
            store.get_task(id).unwrap().unwrap().pane_id.is_none()
        })
        .await;
        store.get_task(id).unwrap().unwrap()
    }

    /// A failed task that asked for it loses its worktree after
    /// `close_failed_after`, through herdr and never forced, which takes
    /// the pane with it. The row stays failed with its error, so `task
    /// retry` still takes it, and the retry puts the checkout back on the
    /// same branch rather than starting a fresh `pastor/t-<id>`.
    #[tokio::test]
    async fn a_failed_task_that_asked_loses_its_worktree_and_its_retry_keeps_the_branch() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected_closing_failed(&fake, &store).await;
        let failed = run_to_failed(&h, &fake, &store, &removing_task(&store, Place::Repo)).await;
        let checkout = failed.spec.checkout.clone().unwrap();
        let ws = failed.workspace_id.clone().unwrap();

        let row = pane_gone(&store, failed.id).await;
        assert_eq!(row.state, TaskState::Failed);
        assert_eq!(row.workspace_id, None);
        assert_eq!(row.error, failed.error);
        assert_eq!(
            calls(&fake, "worktree.remove"),
            vec![serde_json::json!({"workspace_id": ws, "force": false})]
        );
        let own = serde_json::json!({"pane_id": failed.pane_id.clone().unwrap()});
        assert!(!calls(&fake, "pane.close").contains(&own));
        assert!(fake.worktree_list("/r").await.unwrap().is_empty());
        assert!(fake.workspaces().is_empty());

        let retry = store.insert_retry(failed.id).unwrap();
        assert!(retry.spec.removes_worktree());
        let t = h.dispatch(retry.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
        assert_eq!(
            fake.restored(),
            vec![("/r".into(), checkout.path.clone(), checkout.branch.clone())]
        );
        assert_eq!(calls(&fake, "worktree.create").len(), 1);
        assert_eq!(calls(&fake, "worktree.open")[0]["branch"], checkout.branch);
        assert_eq!(t.spec.checkout.unwrap().branch, checkout.branch);
    }

    /// A failed task that did not ask keeps its worktree, as before: only
    /// its pane goes.
    #[tokio::test]
    async fn a_failed_task_that_did_not_ask_keeps_its_worktree() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected_closing_failed(&fake, &store).await;
        let failed =
            run_to_failed(&h, &fake, &store, &placed_task(&store, Place::Repo, true)).await;
        let row = pane_gone(&store, failed.id).await;
        assert_eq!(row.state, TaskState::Failed);
        assert!(calls(&fake, "worktree.remove").is_empty());
        let own = serde_json::json!({"pane_id": failed.pane_id.clone().unwrap()});
        assert!(calls(&fake, "pane.close").contains(&own));
        assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1);
    }

    /// The guards of `task close --remove-worktree` hold for a failed task
    /// too: a dirty checkout, one another agent works in and one a
    /// workspace pastor did not open showed all stay, each with a note
    /// after the task's own error. Its pane goes all the same, and the row
    /// stays failed.
    #[tokio::test]
    async fn a_failed_task_s_worktree_is_kept_by_the_same_guards() {
        for guard in ["dirty", "occupied", "foreign"] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected_closing_failed(&fake, &store).await;
            let place = if guard == "occupied" {
                Place::Pastor
            } else {
                Place::Repo
            };
            if guard == "dirty" {
                fake.dirty_worktrees(true);
            }
            let t = h.dispatch(removing_task(&store, place).id).await.unwrap();
            let checkout = t.spec.checkout.clone().unwrap();
            let why = match guard {
                "dirty" => "worktree kept: uncommitted changes".to_string(),
                "occupied" => {
                    // A fix round working in the checkout from the shared
                    // workspace.
                    let mut fix = placed_task(&store, Place::Pastor, false);
                    fix.spec.repo = Some(checkout.path.clone());
                    store.update_task(&mut fix).unwrap();
                    let fix = h.dispatch(fix.id).await.unwrap();
                    format!("{} works in it", fix.display_id())
                }
                _ => {
                    let mut row = store.get_task(t.id).unwrap().unwrap();
                    row.spec.checkout.as_mut().unwrap().already_open = true;
                    store.update_task(&mut row).unwrap();
                    "a workspace pastor did not open showed it".to_string()
                }
            };
            fake.exit_pane(t.pane_id.as_deref().unwrap());
            wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
            let error = store.get_task(t.id).unwrap().unwrap().error.unwrap();

            let row = pane_gone(&store, t.id).await;
            assert_eq!(row.state, TaskState::Failed, "{guard}");
            let note = row.error.clone().unwrap();
            assert!(note.starts_with(&format!("{error}; ")), "{guard}: {note}");
            assert!(note.contains(&why), "{guard}: {note}");
            assert!(note.contains(&checkout.branch), "{guard}: {note}");
            assert!(fake.removed_checkouts().is_empty(), "{guard}");
            assert_eq!(fake.worktree_list("/r").await.unwrap().len(), 1, "{guard}");
            store.insert_retry(t.id).expect("still retryable");
        }
    }

    /// A stale task's agent may still be at work: its worktree stays while
    /// it works, and goes once it stops and `close_failed_after` has passed,
    /// the task failed first.
    #[tokio::test]
    async fn a_stale_task_s_worktree_goes_only_once_its_agent_stops() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected_closing_failed(&fake, &store).await;
        let t = h
            .dispatch(removing_task(&store, Place::Repo).id)
            .await
            .unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        let mut row = store.get_task(t.id).unwrap().unwrap();
        row.state = TaskState::Stale;
        row.finished_at = Some(Utc::now());
        // A baseline no idle reaches: a stale task's agent has not finished.
        row.last_completion_seq = Some(1_000);
        store.update_task(&mut row).unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(calls(&fake, "worktree.remove").is_empty());
        assert_eq!(state_of(&store, t.id), TaskState::Stale);

        fake.set_status(&pane, AgentStatus::Idle);
        let row = pane_gone(&store, t.id).await;
        assert_eq!(row.state, TaskState::Failed);
        assert_eq!(row.workspace_id, None);
        assert_eq!(calls(&fake, "worktree.remove").len(), 1);
        assert!(fake.worktree_list("/r").await.unwrap().is_empty());
    }

    /// A done task that asked for its worktree to go loses it even with
    /// commits on no remote: they stay on its branch, which is kept.
    #[tokio::test(start_paused = true)]
    async fn a_done_task_that_asked_loses_its_worktree_with_unpushed_commits() {
        for asked in [false, true] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            let task = if asked {
                removing_task(&store, Place::Repo)
            } else {
                placed_task(&store, Place::Repo, true)
            };
            let t = h.dispatch(task.id).await.unwrap();
            fake.set_unpushed(&t.spec.checkout.clone().unwrap().path);
            fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
            auto_closed(&store, t.id).await;
            let row = store.get_task(t.id).unwrap().unwrap();
            assert_eq!(
                calls(&fake, "worktree.remove").len(),
                usize::from(asked),
                "asked {asked}"
            );
            assert_eq!(
                row.error
                    .as_deref()
                    .is_some_and(|e| e.contains("commits on no remote")),
                !asked,
                "{row:?}"
            );
        }
    }

    /// `close_failed_after = "never"` keeps a failed task's pane, as before.
    #[tokio::test]
    async fn never_keeps_a_failed_tasks_pane() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let t = stopped_task(&fake, &store, TaskState::Failed).await;
        let pane = t.pane_id.clone().unwrap();
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: None,
                ..close_failed_settings()
            },
        );
        wait_for("orphan", || !h.snapshot().orphans.is_empty()).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(pane_open(&fake, &pane));
        assert!(calls(&fake, "pane.close").is_empty());
        assert_eq!(store.get_task(t.id).unwrap().unwrap().pane_id, Some(pane));
    }

    /// Not before the grace period, and not while the agent works: a
    /// dispatch can fail after `agent.start` with its agent still at it. The
    /// grace counts from when it actually stops, not from the failure: an
    /// agent that kept working well past the grace still gets a full grace
    /// of its own once it stops, so the pane must stay a moment longer, not
    /// close the instant it goes idle.
    #[tokio::test]
    async fn a_failed_tasks_working_agent_keeps_its_pane() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let t = stopped_task(&fake, &store, TaskState::Failed).await;
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        let grace = Duration::from_millis(300);
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: Some(grace),
                ..close_failed_settings()
            },
        );
        wait_for("orphan", || !h.snapshot().orphans.is_empty()).await;
        // Well past the grace while still at work: the clock must not have
        // been counting since the failure.
        tokio::time::sleep(grace * 2).await;
        assert!(pane_open(&fake, &pane), "at work");
        fake.set_status(&pane, AgentStatus::Idle);
        // Its own grace has barely started: not yet.
        tokio::time::sleep(grace / 3).await;
        assert!(pane_open(&fake, &pane), "its own grace just started");
        wait_for("pane closed after its own grace", || {
            !pane_open(&fake, &pane)
        })
        .await;
        assert_eq!(state_of(&store, t.id), TaskState::Failed);
    }

    /// An orphan's grace also counts from when its agent actually stops, not
    /// from when it was first found orphaned (`find_orphans`): one still at
    /// work past the grace keeps its pane, and gets a full grace of its own
    /// once it stops.
    #[tokio::test]
    async fn an_orphans_grace_starts_when_it_actually_stops() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pane = start_agent(&fake, "t-97").await;
        fake.set_status(&pane, AgentStatus::Working);
        let grace = Duration::from_millis(300);
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: Some(grace),
                ..close_failed_settings()
            },
        );
        wait_for("orphan found", || !h.snapshot().orphans.is_empty()).await;
        tokio::time::sleep(grace * 2).await;
        assert!(pane_open(&fake, &pane), "at work");
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(grace / 3).await;
        assert!(pane_open(&fake, &pane), "its own grace just started");
        wait_for("orphan closed after its own grace", || {
            !pane_open(&fake, &pane)
        })
        .await;
    }

    /// herdr can start a different agent in a pane whose agent pastor is
    /// already timing: the grace is the agent's, not the pane's. An idle
    /// replacement gets its own full `close_failed_after`, from when it is
    /// first seen, however far the earlier agent's clock had run.
    async fn a_replacement_gets_its_own_grace(earlier: &str) {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pane = start_agent(&fake, earlier).await;
        let grace = Duration::from_millis(600);
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: Some(grace),
                ..close_failed_settings()
            },
        );
        wait_for("orphan found", || !h.snapshot().orphans.is_empty()).await;
        // Most of the earlier agent's grace has run.
        tokio::time::sleep(grace * 2 / 3).await;
        assert!(pane_open(&fake, &pane));
        fake.agent_start("t-96", "claude", &pane, &[])
            .await
            .unwrap();
        wait_for("the replacement is the orphan", || {
            h.snapshot().orphans == vec!["t-96".to_string()]
        })
        .await;
        tokio::time::sleep(grace / 2).await;
        assert!(
            pane_open(&fake, &pane),
            "the replacement's own grace has not run out"
        );
        assert!(calls(&fake, "pane.close").is_empty());
        wait_for("closed after the replacement's own grace", || {
            !pane_open(&fake, &pane)
        })
        .await;
    }

    /// A failed task's pane closed by someone else is no longer its pane:
    /// herdr hands the id out again, and auto-close would close whatever
    /// pane gets it next, an empty shell included. The row lets it go on
    /// `pane_closed`, as a pane-owning task's row does.
    #[tokio::test]
    async fn a_failed_task_lets_go_of_a_pane_closed_under_it() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let t = stopped_task(&fake, &store, TaskState::Failed).await;
        let pane = t.pane_id.clone().unwrap();
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: Some(Duration::from_secs(60)),
                ..close_failed_settings()
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        wait_for("orphan found", || !h.snapshot().orphans.is_empty()).await;
        fake.close_pane(&pane);
        wait_for("the row lets the pane go", || {
            store.get_task(t.id).unwrap().unwrap().pane_id.is_none()
        })
        .await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        assert_eq!(row.error, t.error);
        assert!(calls(&fake, "pane.close").is_empty());
        store.insert_retry(t.id).expect("still retryable");
    }

    /// `find_orphans` swaps one orphan in a pane for another: the clock
    /// starts again.
    #[tokio::test]
    async fn an_orphan_replaced_in_its_pane_gets_its_own_grace() {
        a_replacement_gets_its_own_grace("t-97").await;
    }

    /// A failed task's stopped agent had its grace running when herdr put
    /// an orphan in the pane: the task lets the pane go, and the orphan
    /// does not inherit the task agent's clock.
    #[tokio::test]
    async fn an_orphan_in_a_failed_tasks_reused_pane_gets_its_own_grace() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let t = stopped_task(&fake, &store, TaskState::Failed).await;
        let pane = t.pane_id.clone().unwrap();
        let grace = Duration::from_millis(600);
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: Some(grace),
                ..close_failed_settings()
            },
        );
        wait_for("orphan found", || !h.snapshot().orphans.is_empty()).await;
        tokio::time::sleep(grace * 2 / 3).await;
        assert!(pane_open(&fake, &pane));
        fake.agent_start("t-96", "claude", &pane, &[])
            .await
            .unwrap();
        wait_for("the task lets the pane go", || {
            store.get_task(t.id).unwrap().unwrap().pane_id.is_none()
        })
        .await;
        tokio::time::sleep(grace / 2).await;
        assert!(
            pane_open(&fake, &pane),
            "the replacement's own grace has not run out"
        );
        assert!(calls(&fake, "pane.close").is_empty());
        wait_for("closed after the replacement's own grace", || {
            !pane_open(&fake, &pane)
        })
        .await;
        assert_eq!(state_of(&store, t.id), TaskState::Failed);
    }

    /// Two failed tasks' panes come due in one check. While the first
    /// `pane.close` is in flight, the other agent goes back to work: the
    /// agents are listed again before the second close, and that pane is
    /// kept, its grace starting over once it stops.
    #[tokio::test]
    async fn a_pane_whose_agent_resumed_during_an_earlier_close_is_kept() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let a = stopped_task(&fake, &store, TaskState::Failed).await;
        let b = stopped_task(&fake, &store, TaskState::Failed).await;
        // Whichever closes first sets the other working.
        for t in [&a, &b] {
            fake.on_next_close_set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Working);
        }
        let grace = Duration::from_millis(300);
        let (_h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: Some(grace),
                ..close_failed_settings()
            },
        );
        let open = |t: &Task| pane_open(&fake, t.pane_id.as_deref().unwrap());
        wait_for("one pane closed", || !open(&a) || !open(&b)).await;
        let kept = if open(&a) { &a } else { &b };
        tokio::time::sleep(grace * 3).await;
        assert!(open(kept), "its agent is at work again");
        assert_eq!(calls(&fake, "pane.close").len(), 1);
        assert_eq!(
            store.get_task(kept.id).unwrap().unwrap().pane_id,
            kept.pane_id
        );
    }

    /// Two orphans' panes come due in one check. While the first
    /// `pane.close` is in flight, herdr hands the other pane to a new
    /// agent: the agents are listed again before the second close, and the
    /// newcomer keeps the pane for a full grace of its own.
    #[tokio::test]
    async fn a_pane_reused_during_an_earlier_close_is_kept() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pa = start_agent(&fake, "t-97").await;
        let pb = start_agent(&fake, "t-98").await;
        let grace = Duration::from_millis(600);
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: Some(grace),
                ..close_failed_settings()
            },
        );
        wait_for("orphans found", || h.snapshot().orphans.len() == 2).await;
        // Whichever closes first has the other pane handed out again.
        fake.on_next_close_replace(&pa, "t-96");
        fake.on_next_close_replace(&pb, "t-96");
        wait_for("one pane closed", || {
            !pane_open(&fake, &pa) || !pane_open(&fake, &pb)
        })
        .await;
        let kept = if pane_open(&fake, &pa) { pa } else { pb };
        assert_eq!(calls(&fake, "pane.close").len(), 1);
        tokio::time::sleep(grace / 2).await;
        assert!(
            pane_open(&fake, &kept),
            "the newcomer's own grace has not run out"
        );
        wait_for("closed after the newcomer's own grace", || {
            !pane_open(&fake, &kept)
        })
        .await;
    }

    /// A stale task's real completion still gets its full `settle` window
    /// even when `close_failed_after` is shorter, as it is by default (5s
    /// vs 10s): `auto_close_stopped` must not fail the task and close its
    /// pane out from under `confirm_pending_done` while a completion it saw
    /// idle is still pending confirmation.
    #[tokio::test]
    async fn a_stale_tasks_pending_completion_is_not_raced_by_close_failed_after() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(400);
        let close_failed_after = Duration::from_millis(100);
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                settle,
                reconcile_every: Duration::from_millis(50),
                close_failed_after: Some(close_failed_after),
                ..close_failed_settings()
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("activity seen", || {
            store.get_task(t.id).unwrap().unwrap().activity_seen
        })
        .await;
        // A timeout while it is still at work, as `reconcile` would mark it;
        // set directly here to control the exact timing against the idle
        // that follows.
        let mut stale = store.get_task(t.id).unwrap().unwrap();
        stale.state = TaskState::Stale;
        store.update_task(&mut stale).unwrap();
        // It finishes right after timing out.
        fake.set_status(&pane, AgentStatus::Idle);
        // Comfortably past `close_failed_after`, but still inside `settle`:
        // its completion must not be raced into a failure.
        tokio::time::sleep(close_failed_after * 3).await;
        assert!(
            pane_open(&fake, &pane),
            "kept while its completion is pending"
        );
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Stale,
            "not failed out from under its pending completion"
        );
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert_eq!(count(&mut events, "task.done", t.id).len(), 1);
    }

    /// A stale task whose agent stopped is failed and its pane closed, which
    /// frees its slot; `task retry` still takes it. One whose agent is still
    /// at work past its timeout is left stale.
    #[tokio::test]
    async fn a_stale_tasks_stopped_agent_is_closed_and_the_task_failed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let idle = stopped_task(&fake, &store, TaskState::Stale).await;
        let working = stopped_task(&fake, &store, TaskState::Stale).await;
        let working_pane = working.pane_id.clone().unwrap();
        fake.set_status(&working_pane, AgentStatus::Working);
        let (h, mut events) = spawn_with_settings(&fake, &store, close_failed_settings());
        let pane = idle.pane_id.clone().unwrap();
        wait_for("pane closed", || !pane_open(&fake, &pane)).await;
        wait_for("failed", || state_of(&store, idle.id) == TaskState::Failed).await;
        let row = store.get_task(idle.id).unwrap().unwrap();
        assert_eq!(row.pane_id, None);
        assert!(
            row.error.as_deref().unwrap().contains("close_failed_after"),
            "{:?}",
            row.error
        );
        assert_eq!(count(&mut events, "task.failed", idle.id).len(), 1);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(state_of(&store, idle.id), TaskState::Failed, "not reopened");
        assert_eq!(state_of(&store, working.id), TaskState::Stale);
        assert!(pane_open(&fake, &working_pane));
        assert_eq!(h.snapshot().live, 1);
        store.insert_retry(idle.id).expect("still retryable");
    }

    /// `auto_close_stopped` has the same reused-pane guard as `task close`
    /// (`close_leaves_a_pane_reused_by_another_agent`): a stale task's
    /// recorded pane now holds another, still-owned task's agent, idle. It
    /// must not keep claiming that pane and its slot forever, and must not
    /// close the other task's pane; instead the stale row is failed and its
    /// pane id cleared, same as a stopped agent with no replacement.
    #[tokio::test]
    async fn a_stale_tasks_reused_pane_is_failed_without_closing_it() {
        reused_pane_is_repaired(AgentStatus::Idle, 1_000).await;
    }

    /// The replacement's status is not the stale task's: one at work must
    /// not keep the stale row holding the pane and its slot.
    #[tokio::test]
    async fn a_stale_tasks_pane_reused_by_a_working_agent_is_failed() {
        reused_pane_is_repaired(AgentStatus::Working, 1_000).await;
    }

    /// Nor is the replacement's sequence: an idle one past the stale task's
    /// baseline, after activity was seen, must not read as that task's
    /// pending completion and hold off the repair.
    #[tokio::test]
    async fn a_stale_tasks_pane_reused_by_an_idle_agent_past_its_baseline_is_failed() {
        reused_pane_is_repaired(AgentStatus::Idle, 0).await;
    }

    /// The replacement's idle is not the stale task's completion either on
    /// reconcile's side: past the task's baseline, after activity was seen,
    /// it would look like one, and `confirm_pending_done` could mark the
    /// stale task done before `auto_close_stopped` repairs it (what made
    /// the test above flaky under load). With auto-close off, nothing
    /// repairs it, so it must simply stay stale.
    #[tokio::test]
    async fn a_stale_task_never_takes_its_reused_panes_idle_as_its_completion() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (stale, other_pane) = stale_in_reused_pane(&fake, &store, AgentStatus::Idle, 0).await;
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_failed_after: None,
                ..close_failed_settings()
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let before = lists(&fake);
        wait_for("a few settle windows", || lists(&fake) >= before + 6).await;
        assert_eq!(state_of(&store, stale.id), TaskState::Stale);
        assert!(count(&mut events, "task.done", stale.id).is_empty());
        assert!(pane_open(&fake, &other_pane));
    }

    /// A stale task whose recorded pane now holds another, still-open
    /// task's agent in `status`, the stale task's baseline at `baseline`.
    async fn stale_in_reused_pane(
        fake: &FakeHerdr,
        store: &Store,
        status: AgentStatus,
        baseline: u64,
    ) -> (Task, String) {
        let mut stale = new_task(store);
        // A second, still-open task now legitimately holds the pane the
        // stale task remembers: not an orphan, so nothing but this task's
        // own logic could ever close its pane.
        let mut other = new_task(store);
        let other_pane = start_agent(fake, &Task::agent_name_for(other.id)).await;
        fake.set_status_silently(&other_pane, status);
        other.state = TaskState::Running;
        other.machine = Some("m".into());
        other.pane_id = Some(other_pane.clone());
        other.agent_name = Some(Task::agent_name_for(other.id));
        other.started_at = Some(Utc::now());
        store.update_task(&mut other).unwrap();
        stale.state = TaskState::Stale;
        stale.machine = Some("m".into());
        stale.pane_id = Some(other_pane.clone());
        stale.agent_name = Some(Task::agent_name_for(stale.id));
        stale.started_at = Some(Utc::now());
        stale.finished_at = Some(Utc::now());
        stale.last_completion_seq = Some(baseline);
        stale.activity_seen = true;
        store.update_task(&mut stale).unwrap();
        (stale, other_pane)
    }

    async fn reused_pane_is_repaired(status: AgentStatus, baseline: u64) {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (stale, other_pane) = stale_in_reused_pane(&fake, &store, status, baseline).await;
        let (h, mut events) = spawn_with_settings(&fake, &store, close_failed_settings());
        wait_for("failed", || state_of(&store, stale.id) == TaskState::Failed).await;
        let row = store.get_task(stale.id).unwrap().unwrap();
        assert_eq!(row.pane_id, None);
        assert_eq!(count(&mut events, "task.failed", stale.id).len(), 1);
        // A few more checks past the grace: the other task's pane stays
        // open and untouched, and the stale row does not flip back or
        // reopen.
        let before = lists(&fake);
        wait_for("a few more reconciles", || lists(&fake) >= before + 3).await;
        assert!(calls(&fake, "pane.close").is_empty());
        assert!(pane_open(&fake, &other_pane));
        assert_eq!(fake.agents().len(), 1);
        assert_eq!(fake.agents()[0].pane_id, other_pane);
        assert_eq!(state_of(&store, stale.id), TaskState::Failed);
        assert!(h.snapshot().orphans.is_empty());
        store.insert_retry(stale.id).expect("still retryable");
    }

    /// Agents named `t-<id>` that no open task owns (a closed row whose pane
    /// stayed, a row that is gone) are closed once stopped and orphaned for
    /// the grace period. Anything not named like a task is never touched,
    /// and neither is an orphan at work.
    #[tokio::test]
    async fn an_orphaned_pane_is_found_and_closed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut closed = new_task(&store);
        closed.state = TaskState::Closed;
        closed.machine = Some("m".into());
        store.update_task(&mut closed).unwrap();
        let closed_pane = start_agent(&fake, &Task::agent_name_for(closed.id)).await;
        let ghost_pane = start_agent(&fake, "t-99").await;
        let busy_pane = start_agent(&fake, "t-98").await;
        fake.set_status(&busy_pane, AgentStatus::Working);
        let mine = start_agent(&fake, "mine").await;
        let (h, _events) = spawn_with_settings(&fake, &store, close_failed_settings());
        wait_for("orphans closed", || {
            !pane_open(&fake, &closed_pane) && !pane_open(&fake, &ghost_pane)
        })
        .await;
        wait_for("one orphan left", || {
            h.snapshot().orphans == vec!["t-98".to_string()]
        })
        .await;
        assert!(pane_open(&fake, &busy_pane));
        assert!(pane_open(&fake, &mine));
        assert_eq!(state_of(&store, closed.id), TaskState::Closed);
        assert_eq!(h.snapshot().live, 1);
    }

    /// An agent that ends its task mid-turn (`pastor task done`): the task
    /// is done at once and stays so while the agent works on, and auto-close
    /// frees its pane once the agent is idle, although that turn moved the
    /// sequence past the baseline.
    #[tokio::test(start_paused = true)]
    async fn an_ended_task_is_closed_once_its_agent_is_idle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("activity seen", || {
            store.get_task(t.id).unwrap().unwrap().activity_seen
        })
        .await;
        let ended = h.end(t.id, None).await.unwrap();
        assert_eq!(ended.state, TaskState::Done);
        assert!(ended.ended);
        assert_eq!(count(&mut events, "task.done", t.id).len(), 1);
        // Ending again changes nothing and says nothing.
        assert!(h.end(t.id, None).await.unwrap().ended);
        // Past its grace: only the agent at work keeps the pane.
        finished_an_hour_ago(&store, t.id);
        let before = lists(&fake);
        wait_for("a few reconciles", || lists(&fake) >= before + 4).await;
        assert_eq!(state_of(&store, t.id), TaskState::Done, "still at work");
        assert!(calls(&fake, "pane.close").is_empty());
        assert_eq!(h.snapshot().live, 1, "the slot is held while it works");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        assert_eq!(
            calls(&fake, "pane.close"),
            vec![serde_json::json!({"pane_id": pane})]
        );
        assert!(count(&mut events, "task.done", t.id).is_empty());
        wait_for("live drops", || h.snapshot().live == 0).await;
    }

    /// `task done --summary` ends the round with what the agent said; said
    /// again, it replaces that round's rather than starting another.
    #[tokio::test(start_paused = true)]
    async fn a_task_ended_with_a_summary_keeps_it_for_its_round() {
        use crate::task::{Outcome, SummarySource};
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let ended = h
            .end(t.id, Some("partial: tests left\npushed pastor/t-1".into()))
            .await
            .unwrap();
        let summary = ended.summary.expect("the reply carries the summary");
        assert_eq!(summary.round, 1);
        assert_eq!(summary.outcome, Outcome::Partial);
        assert_eq!(summary.source, SummarySource::Agent);
        assert_eq!(count(&mut events, "task.done", t.id).len(), 1);
        let again = h.end(t.id, Some("done".into())).await.unwrap();
        assert_eq!(again.summary.unwrap().outcome, Outcome::Done);
        let all = store.summaries(t.id).unwrap();
        assert_eq!(all.len(), 1, "{all:?}");
        assert!(count(&mut events, "task.done", t.id).is_empty());
    }

    /// A task pastor finds done on its own ends a round with no summary,
    /// holding the pane's last lines; the tail stays for the finish command,
    /// and a summary the agent sends after replaces that round's.
    #[tokio::test(start_paused = true)]
    async fn a_round_with_no_summary_keeps_the_end_of_the_pane() {
        use crate::task::{Outcome, SummarySource};
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(100)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● Opened https://example.org/pr/7\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        let done = store.get_task(t.id).unwrap().unwrap();
        let summary = done.summary.expect("a done task shows its round");
        assert_eq!(summary.outcome, Outcome::NoSummary);
        assert_eq!(summary.source, SummarySource::Pane);
        assert!(summary.text.contains("example.org/pr/7"), "{summary:?}");
        assert!(store.take_pane_tail(t.id).is_some(), "left for the finish");
        let late = h.end(t.id, Some("done: PR 7".into())).await.unwrap();
        let late = late.summary.unwrap();
        assert_eq!((late.round, late.outcome), (1, Outcome::Done));
        assert_eq!(late.source, SummarySource::Agent);
    }

    /// On an `ssh` machine of a head with no `head_address`, an agent's
    /// `pastor task done` cannot reach the head, so a round there ends with
    /// no summary whatever the agent did. It says `no head`, not `no
    /// summary`, so an orchestrator does not read it as unfinished work. A
    /// task that asked for no summary still ends as `no summary`.
    #[tokio::test(start_paused = true)]
    async fn a_round_on_a_machine_cut_off_from_the_head_is_no_head() {
        use crate::task::{Outcome, SummaryMode, SummarySource};
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                cut_off_from_head: true,
                ..settings_with_settle(Duration::from_millis(100))
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let off = store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    summary: SummaryMode::Off,
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap();
        for (task, want) in [
            (new_task(&store), Outcome::NoHead),
            (off, Outcome::NoSummary),
        ] {
            let t = h.dispatch(task.id).await.unwrap();
            let pane = t.pane_id.clone().unwrap();
            fake.set_status(&pane, AgentStatus::Working);
            fake.set_pane_text(&pane, "● Opened https://example.org/pr/7\n\n❯\n");
            fake.set_status(&pane, AgentStatus::Idle);
            wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
            let summary = store.get_task(t.id).unwrap().unwrap().summary.unwrap();
            assert_eq!(summary.outcome, want, "{:?}", task.spec.summary);
            assert_eq!(summary.source, SummarySource::Pane);
            assert!(summary.text.contains("example.org/pr/7"), "{summary:?}");
        }
    }

    /// The bundle gate on an actor: a task whose machine's bundles will not
    /// let it start fails with `bundle_stale` and the reason, and starts
    /// nothing; one the gate lets through starts.
    #[tokio::test(start_paused = true)]
    async fn a_task_the_bundle_gate_refuses_fails_with_bundle_stale() {
        use crate::bundle::keep::{Gate, GateHandle};
        use std::future::Future;
        use std::pin::Pin;
        struct Said(Result<(), String>);
        impl Gate for Said {
            fn before_launch<'a>(
                &'a self,
                _machine: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
                let answer = self.0.clone();
                Box::pin(async move { answer })
            }
        }
        for (answer, starts) in [
            (
                Err("the last bundle apply to m failed: boom".to_string()),
                false,
            ),
            (Ok(()), true),
        ] {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let settings = MachineSettings {
                bundle_gate: GateHandle(Some(Arc::new(Said(answer)))),
                ..MachineSettings::default()
            };
            let (h, _events) = spawn_with_settings(&fake, &store, settings);
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            let task = new_task(&store);
            let started = h.dispatch(task.id).await;
            let row = store.get_task(task.id).unwrap().unwrap();
            let started_one = fake.requests().iter().any(|r| r.method == "agent.start");
            assert_eq!(started_one, starts);
            if starts {
                started.unwrap();
                assert_eq!(row.state, TaskState::Running);
            } else {
                let err = started.unwrap_err();
                assert!(err.to_string().contains("bundle_stale"), "{err}");
                assert_eq!(row.state, TaskState::Failed);
                let error = row.error.unwrap();
                assert!(error.starts_with("bundle_stale: "), "{error}");
                assert!(error.contains("boom"), "{error}");
                assert!(row.finished_at.is_some());
            }
        }
    }

    /// A task whose `summary` is `require`.
    fn new_required_task(store: &Store) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    summary: crate::task::SummaryMode::Require,
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    /// With `require`, the agent's own `task done` without a summary is
    /// refused and leaves the task as it was; with one it ends `done`.
    #[tokio::test(start_paused = true)]
    async fn a_required_summary_refuses_the_agents_bare_task_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_required_task(&store).id).await.unwrap();
        let err = h.end(t.id, None).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<SendRefused>().map(|r| r.code),
            Some(crate::task::SUMMARY_REQUIRED)
        );
        assert!(err.to_string().contains("--summary '<summary>'"), "{err}");
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(store.summaries(t.id).unwrap().is_empty());
        let done = h
            .end(t.id, Some("blocked: no access".into()))
            .await
            .unwrap();
        assert_eq!(done.state, TaskState::Done);
        assert_eq!(
            done.summary.unwrap().outcome,
            crate::task::Outcome::Blocked,
            "any outcome counts"
        );
        // Ended with one: a bare `task done` after it keeps it.
        let again = h.end(t.id, None).await.unwrap();
        assert_eq!(
            again.summary.unwrap().outcome,
            crate::task::Outcome::Blocked
        );
    }

    /// With `require`, a person's `task done t-N` is not refused; the round
    /// says it was ended by hand.
    #[tokio::test(start_paused = true)]
    async fn a_required_summary_lets_a_person_end_the_task() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_required_task(&store).id).await.unwrap();
        let done = h.end_by(t.id, None, EndBy::Hand).await.unwrap();
        assert_eq!(done.state, TaskState::Done);
        let round = done.summary.unwrap();
        assert_eq!(round.outcome, crate::task::Outcome::NoSummary);
        assert_eq!(round.text, crate::task::ENDED_BY_HAND);
    }

    /// With `require`, an agent pastor finds idle and finished without a
    /// summary ends the task `failed`, with the pane's last lines kept;
    /// with `ask` the same finish is `done`.
    #[tokio::test(start_paused = true)]
    async fn a_required_summary_fails_an_idle_finish_without_one() {
        use crate::task::{Outcome, SummarySource};
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(100)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_required_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● All finished\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        let failed = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            failed.error.as_deref(),
            Some(crate::task::STOPPED_WITHOUT_SUMMARY)
        );
        let round = failed.summary.expect("a failed task shows its round");
        assert_eq!(round.outcome, Outcome::NoSummary);
        assert_eq!(round.source, SummarySource::Pane);
        assert!(round.text.contains("All finished"), "{round:?}");
    }

    /// Typing into an ended task gives it more to do: it runs again, and no
    /// longer counts as ended.
    #[tokio::test(start_paused = true)]
    async fn sending_to_an_ended_task_reopens_it() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(150);
        let (h, _events) = spawn_with_settings(&fake, &store, settings_with_settle(settle));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        let typed = || {
            let sent = calls(&fake, "pane.send_text");
            sent.last().unwrap()["text"].as_str().unwrap().to_string()
        };
        // A reply to a running task: nothing added.
        let reply = SendInput {
            text: Some("carry on".into()),
            enter: true,
            ..Default::default()
        };
        h.send(t.id, reply).await.unwrap();
        assert_eq!(typed(), "carry on");
        h.end(t.id, None).await.unwrap();
        // The rest of the turn after `task done`: work, then idle.
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("activity recorded", || {
            store.get_task(t.id).unwrap().unwrap().activity_seen
        })
        .await;
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 3).await;
        let sent = h
            .send(
                t.id,
                SendInput {
                    text: Some("one more thing".into()),
                    enter: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(sent.state, TaskState::Running);
        // New work asks for a summary again.
        assert_eq!(
            typed(),
            format!("one more thing {}", crate::task::SUMMARY_ASK)
        );
        assert!(!sent.ended);
        assert!(!store.get_task(t.id).unwrap().unwrap().ended);
        // An idle report before the agent picks the input up must not count
        // the ended turn's work as the new turn's: the task keeps running.
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(settle * 4).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        // The new turn's own work finishes it.
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// Only a task with a pane on this machine can end.
    #[tokio::test(start_paused = true)]
    async fn a_task_with_no_pane_cannot_end() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let queued = new_task(&store);
        let err = h.end(queued.id, None).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<SendRefused>().map(|r| r.code),
            Some("task_not_live"),
            "{err:#}"
        );
        assert_eq!(state_of(&store, queued.id), TaskState::Queued);
        let err = h.end(999, None).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<SendRefused>().map(|r| r.code),
            Some("task_not_found"),
            "{err:#}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn auto_close_emits_task_closed_once() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let task = store
            .insert_task(NewTask {
                description: None,
                job: "nightly".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: spec(),
                flock: "default".into(),
            })
            .unwrap();
        let t = run_to_done(&h, &fake, &store, task).await;
        auto_closed(&store, t.id).await;
        // Two more reconcile passes, and the pane.closed event herdr sends.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let closed = count(&mut events, "task.closed", t.id);
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert_eq!(closed[0].job.as_deref(), Some("nightly"));
        assert_eq!(closed[0].machine.as_deref(), Some("m"));
        assert_eq!(calls(&fake, "pane.close").len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn never_disables_auto_close() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                close_done_after: None,
                ..auto_close_settings()
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = run_to_done(&h, &fake, &store, new_task(&store)).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert!(calls(&fake, "pane.close").is_empty());
    }

    /// Failed, blocked and stale tasks are left for `task retry` and `task
    /// close`, however long ago they stopped.
    #[tokio::test(start_paused = true)]
    async fn only_done_tasks_are_auto_closed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let long_ago = Utc::now() - chrono::Duration::hours(1);
        let mut ids = vec![];
        // Each agent shows the status that keeps its task where it is, so
        // only auto-close could move it.
        for (state, status) in [
            (TaskState::Failed, AgentStatus::Idle),
            (TaskState::Blocked, AgentStatus::Blocked),
            (TaskState::Stale, AgentStatus::Working),
        ] {
            let mut t = new_task(&store);
            let pane = start_agent(&fake, &Task::agent_name_for(t.id)).await;
            fake.set_status_silently(&pane, status);
            t.state = state;
            t.machine = Some("m".into());
            t.pane_id = Some(pane);
            t.agent_name = Some(Task::agent_name_for(t.id));
            t.started_at = Some(Utc::now());
            t.finished_at = Some(long_ago);
            store.update_task(&mut t).unwrap();
            ids.push((t.id, state));
        }
        let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        for (id, state) in ids {
            assert_eq!(state_of(&store, id), state);
        }
        assert!(calls(&fake, "pane.close").is_empty());
        assert!(calls(&fake, "worktree.remove").is_empty());
    }

    fn kept_task(store: &Store) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    keep_pane: Some(true),
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    /// A task that keeps its pane goes done like any other, but auto-close
    /// leaves its pane and worktree, well past `close_done_after`, and it
    /// holds no slot while done. `task close` still closes it, and `task
    /// send` sets it running again, holding a slot once more.
    #[tokio::test(start_paused = true)]
    async fn a_kept_done_task_keeps_its_pane_and_frees_its_slot() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = run_to_done(&h, &fake, &store, kept_task(&store)).await;
        finished_an_hour_ago(&store, t.id);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert!(calls(&fake, "pane.close").is_empty());
        assert!(calls(&fake, "worktree.remove").is_empty());
        wait_for("no slot held", || h.snapshot().live == 0).await;
        let sent = h
            .send(
                t.id,
                SendInput {
                    text: Some("one more thing".into()),
                    enter: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(sent.state, TaskState::Running);
        wait_for("slot held again", || h.snapshot().live == 1).await;
        let closed = h.close(t.id, false).await.unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(
            calls(&fake, "pane.close"),
            vec![serde_json::json!({"pane_id": t.pane_id.clone().unwrap()})]
        );
    }

    /// A failed or stale task that keeps its pane keeps it past
    /// `close_failed_after`, and its row is left as it is.
    #[tokio::test]
    async fn a_kept_failed_or_stale_tasks_pane_stays() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut kept = vec![];
        for state in [TaskState::Failed, TaskState::Stale] {
            let mut t = stopped_task(&fake, &store, state).await;
            t.spec.keep_pane = Some(true);
            store.update_task(&mut t).unwrap();
            kept.push(t);
        }
        // A plain failed task beside them shows the check has run.
        let plain = stopped_task(&fake, &store, TaskState::Failed).await;
        let plain_pane = plain.pane_id.clone().unwrap();
        let (_h, _events) = spawn_with_settings(&fake, &store, close_failed_settings());
        wait_for("plain pane closed", || !pane_open(&fake, &plain_pane)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        for t in kept {
            let pane = t.pane_id.clone().unwrap();
            assert!(pane_open(&fake, &pane), "{:?}", t.state);
            let row = store.get_task(t.id).unwrap().unwrap();
            assert_eq!(row.state, t.state);
            assert_eq!(row.pane_id, Some(pane));
        }
        assert_eq!(calls(&fake, "pane.close").len(), 1);
    }

    /// A kept done task holds no slot; a kept task still at work, or a
    /// done one that does not keep its pane, does.
    #[test]
    fn count_live_leaves_out_a_kept_done_task() {
        let store = Store::open_in_memory().unwrap();
        let task = |state, keep| {
            let mut t = new_task(&store);
            t.state = state;
            t.spec.keep_pane = keep;
            t
        };
        let mut s = pull_machine("m".into(), 4, vec![]).snapshot();
        count_live(
            &mut s,
            &[
                task(TaskState::Done, Some(true)),
                task(TaskState::Done, None),
                task(TaskState::Running, Some(true)),
                task(TaskState::Stale, Some(true)),
            ],
        );
        assert_eq!(s.live, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn auto_close_removes_a_clean_worktree() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = run_to_done(&h, &fake, &store, worktree_task(&store)).await;
        auto_closed(&store, t.id).await;
        assert_eq!(
            calls(&fake, "worktree.remove"),
            vec![
                serde_json::json!({"workspace_id": t.workspace_id.clone().unwrap(), "force": false})
            ]
        );
        assert!(closes(&fake).is_empty());
        assert!(store.get_task(t.id).unwrap().unwrap().error.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn auto_close_keeps_a_dirty_worktree() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        fake.dirty_worktrees(true);
        let t = run_to_done(&h, &fake, &store, worktree_task(&store)).await;
        auto_closed(&store, t.id).await;
        let removes = calls(&fake, "worktree.remove");
        assert_eq!(
            removes,
            vec![
                serde_json::json!({"workspace_id": t.workspace_id.clone().unwrap(), "force": false})
            ],
            "never forced"
        );
        assert_eq!(
            closes(&fake),
            vec![serde_json::json!({"pane_id": t.pane_id.clone().unwrap()})],
            "the pane closes anyway"
        );
        let note = store.get_task(t.id).unwrap().unwrap().error.unwrap();
        assert!(note.contains("worktree kept"), "{note}");
        assert!(note.contains("pastor/t-"), "names the branch: {note}");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(count(&mut events, "task.closed", t.id).len(), 1);
        assert_eq!(calls(&fake, "worktree.remove").len(), 1, "not retried");
    }

    /// A clean checkout with commits on no remote (a push that failed) is
    /// kept like a dirty one: herdr's `worktree.remove` only refuses
    /// uncommitted changes, so pastor asks git first and never calls it.
    #[tokio::test(start_paused = true)]
    async fn auto_close_keeps_a_worktree_with_unpushed_commits() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        let checkout = t
            .spec
            .checkout
            .clone()
            .expect("dispatch records the checkout");
        fake.set_unpushed(&checkout.path);
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        auto_closed(&store, t.id).await;
        assert!(
            calls(&fake, "worktree.remove").is_empty(),
            "the checkout stays"
        );
        assert_eq!(
            closes(&fake),
            vec![serde_json::json!({"pane_id": t.pane_id.clone().unwrap()})],
            "the pane closes anyway"
        );
        let closed = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            closed.workspace_id, t.workspace_id,
            "the checkout is still there"
        );
        let note = closed.error.unwrap();
        assert!(note.contains("worktree kept"), "{note}");
        assert!(note.contains("commits on no remote"), "{note}");
        assert!(note.contains(&checkout.branch), "names the branch: {note}");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(count(&mut events, "task.closed", t.id).len(), 1);
        assert!(calls(&fake, "worktree.remove").is_empty(), "not retried");
    }

    /// A malformed `worktree.remove` reply is a decoding failure, not herdr
    /// refusing the call: `code()` is `None`, so it must not be mistaken for
    /// the "keep the worktree, close the pane anyway" path. Nothing closes;
    /// the next reconcile, with a proper reply, closes it.
    #[tokio::test(start_paused = true)]
    async fn auto_close_retries_after_a_malformed_worktree_remove_reply() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        fake.set_malformed_reply("worktree.remove");
        let t = run_to_done(&h, &fake, &store, worktree_task(&store)).await;
        finished_an_hour_ago(&store, t.id);
        // First reconcile: the reply fails to decode, so nothing closes. Checked
        // right after that one attempt, before the next reconcile (100ms later)
        // can retry with a proper reply and close it.
        wait_for("the malformed attempt", || {
            !calls(&fake, "worktree.remove").is_empty()
        })
        .await;
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert!(closes(&fake).is_empty());
        assert_eq!(
            calls(&fake, "worktree.remove").len(),
            1,
            "the first attempt, malformed"
        );
        // Next reconcile: a proper reply closes it.
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        assert_eq!(calls(&fake, "worktree.remove").len(), 2);
        assert!(store.get_task(t.id).unwrap().unwrap().error.is_none());
    }

    /// A done task whose pane the user already closed in herdr ends closed,
    /// with no error, once, and later passes leave it alone.
    #[tokio::test(start_paused = true)]
    async fn auto_close_tolerates_a_missing_pane() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = worktree_task(&store);
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        t.workspace_id = Some("w77".into());
        t.pane_id = Some("w77:p1".into());
        t.agent_name = Some(Task::agent_name_for(t.id));
        t.last_completion_seq = Some(1);
        t.finished_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.update_task(&mut t).unwrap();
        let (h, mut events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let closed = store.get_task(t.id).unwrap().unwrap();
        assert!(closed.error.is_none(), "{:?}", closed.error);
        assert_eq!(count(&mut events, "task.closed", t.id).len(), 1);
    }

    /// herdr hands out pane ids again: the pane a done task recorded now holds
    /// another agent, idle. Auto-close must neither close that pane nor remove
    /// its workspace, since pastor never closes a pane it did not open for the
    /// task. The row is left `Done`, for reconcile to move on.
    #[tokio::test(start_paused = true)]
    async fn auto_close_skips_a_pane_reused_by_another_agent() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = worktree_task(&store);
        let other = start_agent(&fake, "t-99").await;
        fake.set_status_silently(&other, AgentStatus::Idle);
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        t.workspace_id = Some(other.split(':').next().unwrap().into());
        t.pane_id = Some(other.clone());
        t.agent_name = Some(Task::agent_name_for(t.id));
        t.last_completion_seq = Some(fake.agents()[0].state_change_seq);
        t.finished_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.update_task(&mut t).unwrap();
        let (h, mut events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let before = lists(&fake);
        wait_for("a few reconciles", || lists(&fake) >= before + 4).await;
        assert!(calls(&fake, "pane.close").is_empty());
        assert!(calls(&fake, "worktree.remove").is_empty());
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert_eq!(fake.agents().len(), 1);
        assert_eq!(fake.agents()[0].pane_id, other);
        assert!(count(&mut events, "task.closed", t.id).is_empty());
    }

    /// A done task whose agent ran a whole extra work cycle -- working, then
    /// idle again -- that no reconcile ever caught mid-flight: auto-close's
    /// fresh check finds it idle, matching the row, but its `state_change_seq`
    /// has moved past the row's baseline. Closing now would destroy that
    /// unsettled completion; it must skip and leave the row for reconcile's
    /// settle window instead.
    #[tokio::test(start_paused = true)]
    async fn auto_close_skips_an_agent_whose_sequence_moved() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let pane = start_agent(&fake, &Task::agent_name_for(t.id)).await;
        fake.set_status_silently(&pane, AgentStatus::Idle);
        let baseline = fake.agents()[0].state_change_seq;
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        t.pane_id = Some(pane.clone());
        t.agent_name = Some(Task::agent_name_for(t.id));
        t.last_completion_seq = Some(baseline);
        t.finished_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.update_task(&mut t).unwrap();
        // The unobserved cycle: back to idle, but the sequence moved twice
        // (idle -> working -> idle), with no event and no reconcile in between.
        fake.set_status_silently(&pane, AgentStatus::Working);
        fake.set_status_silently(&pane, AgentStatus::Idle);
        assert!(fake.agents()[0].state_change_seq > baseline);
        let (h, mut events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let before = lists(&fake);
        wait_for("a few reconciles", || lists(&fake) >= before + 4).await;
        assert!(calls(&fake, "pane.close").is_empty());
        assert!(calls(&fake, "worktree.remove").is_empty());
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert_eq!(fake.agents().len(), 1);
        assert!(count(&mut events, "task.closed", t.id).is_empty());
    }

    /// A done row written before pastor recorded a completion baseline has
    /// `last_completion_seq` NULL. Its agent is idle with a nonzero sequence,
    /// as any agent that ever worked is. That must not read as a moved
    /// sequence: the first pass seeds the baseline from herdr and a later
    /// pass closes the task.
    #[tokio::test(start_paused = true)]
    async fn auto_close_closes_a_done_row_without_a_baseline() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let pane = start_agent(&fake, &Task::agent_name_for(t.id)).await;
        fake.set_status_silently(&pane, AgentStatus::Working);
        fake.set_status_silently(&pane, AgentStatus::Idle);
        assert!(fake.agents()[0].state_change_seq > 0);
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        t.pane_id = Some(pane.clone());
        t.agent_name = Some(Task::agent_name_for(t.id));
        t.last_completion_seq = None;
        t.finished_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.update_task(&mut t).unwrap();
        let (h, mut events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let before = lists(&fake);
        wait_for("closed within a few reconciles", || {
            assert!(lists(&fake) < before + 8, "never auto-closed");
            state_of(&store, t.id) == TaskState::Closed
        })
        .await;
        assert_eq!(calls(&fake, "pane.close").len(), 1);
        assert!(fake.agents().is_empty());
        assert_eq!(count(&mut events, "task.closed", t.id).len(), 1);
    }

    /// A done task, past its grace period, whose agent is idle in herdr but
    /// turns `Working` between a reconcile's `agent.list` and the fresh one
    /// auto-close makes before closing. Returns the running machine, its
    /// events, the task as seeded and its pane.
    async fn done_task_that_resumes(
        fake: &FakeHerdr,
        store: &Arc<Store>,
    ) -> (
        MachineHandle,
        broadcast::Receiver<PastorEvent>,
        Task,
        String,
    ) {
        let mut t = new_task(store);
        let pane = start_agent(fake, &Task::agent_name_for(t.id)).await;
        fake.set_status_silently(&pane, AgentStatus::Idle);
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        t.pane_id = Some(pane.clone());
        t.agent_name = Some(Task::agent_name_for(t.id));
        t.last_completion_seq = Some(fake.agents()[0].state_change_seq);
        t.finished_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.update_task(&mut t).unwrap();
        fake.set_status_between_lists(&pane, AgentStatus::Working);
        // The first reconcile tick lists and auto-close lists right after it.
        // A settle window well past that tick keeps the settle check's own
        // `agent.list` from landing next to the reconcile's instead.
        let settings = MachineSettings {
            settle: Duration::from_millis(300),
            ..auto_close_settings()
        };
        let (h, events) = spawn_with_settings(fake, store, settings);
        // Or is closed at work, the bug this guards against.
        wait_for("the agent resumes", || match fake.agents().first() {
            Some(a) => a.agent_status == AgentStatus::Working,
            None => true,
        })
        .await;
        (h, events, t, pane)
    }

    /// Only the fresh `agent.list` saw the agent at work again: auto-close
    /// leaves it, and the next reconcile moves the task back to running.
    #[tokio::test(start_paused = true)]
    async fn auto_close_skips_an_agent_that_resumed_work() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (_h, mut events, t, pane) = done_task_that_resumes(&fake, &store).await;
        wait_for("running again, or closed", || {
            matches!(
                state_of(&store, t.id),
                TaskState::Running | TaskState::Closed
            )
        })
        .await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(calls(&fake, "pane.close").is_empty());
        assert!(calls(&fake, "worktree.remove").is_empty());
        assert_eq!(fake.agents().len(), 1);
        assert_eq!(fake.agents()[0].pane_id, pane);
        assert!(count(&mut events, "task.closed", t.id).is_empty());
    }

    /// The same task closes once its agent is idle again, done again, and
    /// past the grace period from the new finish.
    #[tokio::test(start_paused = true)]
    async fn auto_close_closes_a_resumed_task_once_it_is_done_again() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (_h, mut events, t, pane) = done_task_that_resumes(&fake, &store).await;
        wait_for("running again, or closed", || {
            matches!(
                state_of(&store, t.id),
                TaskState::Running | TaskState::Closed
            )
        })
        .await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done again", || state_of(&store, t.id) == TaskState::Done).await;
        let done = store.get_task(t.id).unwrap().unwrap();
        assert!(done.finished_at > t.finished_at, "a new finish time");
        assert!(calls(&fake, "pane.close").is_empty(), "a new grace period");
        auto_closed(&store, t.id).await;
        assert_eq!(
            calls(&fake, "pane.close"),
            vec![serde_json::json!({"pane_id": pane})]
        );
        assert!(fake.agents().is_empty());
        assert_eq!(count(&mut events, "task.closed", t.id).len(), 1);
    }

    /// Auto-close waits for a connected machine: while polling, reconcile
    /// runs on every poll tick, but a done task is only closed once the
    /// events are back. (A lost machine runs no reconcile at all.)
    #[tokio::test(start_paused = true)]
    async fn a_polling_machine_does_not_auto_close() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let pane = start_agent(&fake, &Task::agent_name_for(t.id)).await;
        fake.set_status_silently(&pane, AgentStatus::Idle);
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        t.pane_id = Some(pane);
        t.agent_name = Some(Task::agent_name_for(t.id));
        // The launch itself is a state change (see `FakeHerdr::agent_start`),
        // so the row's baseline is the sequence already settled at, not 0.
        t.last_completion_seq = Some(fake.agents()[0].state_change_seq);
        t.finished_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.update_task(&mut t).unwrap();
        let (events, _rx) = broadcast::channel(64);
        let mut settings = auto_close_settings();
        settings.poll_every = Duration::from_millis(50);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail_until: 6,
                wedge_forever: false,
                slow_ack: None,
                fake: fake.clone(),
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        let polls = lists(&fake);
        wait_for("a few poll reconciles", || lists(&fake) >= polls + 3).await;
        assert_eq!(h.snapshot().channel, ChannelState::Polling);
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert!(calls(&fake, "pane.close").is_empty());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
    }

    #[tokio::test(start_paused = true)]
    async fn done_is_cancelled_if_agent_resumes_within_settle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(500)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        fake.set_status(&pane, AgentStatus::Working);
        // Long enough to clear the 500ms settle window measured from the original
        // Idle event: if `Working` had *not* cancelled the pending-done entry, the
        // settle check would have marked the task Done well before this returns.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    fn saw(events: &mut broadcast::Receiver<PastorEvent>, kind: &str, id: i64) -> bool {
        let mut found = false;
        while let Ok(ev) = events.try_recv() {
            found |= ev.kind == kind && ev.task_id == Some(id);
        }
        found
    }

    /// The fleet bug of 2026-09-25: herdr 0.9.1 has no `completion_seq`, so a
    /// finished agent only shows as `idle`/`done` with a newer
    /// `state_change_seq`. That alone, after `settle`, must make the task done.
    #[tokio::test(start_paused = true)]
    async fn an_agent_that_works_then_goes_idle_is_done_after_settle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(300)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        assert_eq!(fake.agents()[0].agent_status, AgentStatus::Working);

        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "done waits for the settle window"
        );
        let list = fake.agent_list().await.unwrap();
        assert_eq!(list[0].agent_status, AgentStatus::Idle);
        assert_eq!(list[0].completion_seq, None, "herdr 0.9.1 has none");
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(saw(&mut events, "task.done", t.id), "task.done was emitted");
        assert_eq!(h.snapshot().live, 1, "done keeps its pane until it closes");
    }

    /// The agent finished, sat idle, and the human typed `/exit` before the
    /// settle window confirmed it: the process exiting is the end of work
    /// that was done, not a failure.
    #[tokio::test(start_paused = true)]
    async fn an_idle_agent_that_exits_leaves_its_task_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) =
            spawn_with_settings(&fake, &store, settings_with_settle(Duration::from_secs(30)));
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        fake.exit_pane(&pane);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        let row = store.get_task(t.id).unwrap().unwrap();
        assert!(row.error.is_none(), "{:?}", row.error);
        assert!(row.finished_at.is_some());
        assert!(saw(&mut events, "task.done", t.id));
    }

    /// An agent that exits while working crashed, or was killed: that task
    /// did fail.
    #[tokio::test(start_paused = true)]
    async fn a_working_agent_that_exits_fails_its_task() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        fake.set_status(&pane, AgentStatus::Working);
        tokio::time::sleep(Duration::from_millis(50)).await;
        fake.exit_pane(&pane);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().error.as_deref(),
            Some("agent process exited")
        );
    }

    /// The pending prompt goes in on an `idle` sighting, and herdr answers
    /// that the agent is working on it. Without an event stream nothing else
    /// tells the actor, so the reply itself must: an exit after it is an exit
    /// while working, a failure, not the end of work that was done.
    #[tokio::test(start_paused = true)]
    async fn an_exit_after_the_pending_prompt_went_in_is_not_an_idle_completion() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(150));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let mut settings = settings();
        settings.poll_every = Duration::from_millis(50);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail_until: usize::MAX,
                wedge_forever: false,
                slow_ack: None,
                fake: fake.clone(),
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.prompt_pending);
        let pane = t.pane_id.clone().unwrap();

        fake.set_status_silently(&pane, AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(fake.agents()[0].agent_status, AgentStatus::Working);

        fake.exit_pane(&pane);
        wait_for("failed", || {
            matches!(
                state_of(&store, t.id),
                TaskState::Failed | TaskState::Done | TaskState::Closed
            )
        })
        .await;
        assert_eq!(state_of(&store, t.id), TaskState::Failed);
    }

    /// The dogfooding bug: the agent worked, finished and sat idle waiting
    /// for input, and the task stayed `running` until the session ended.
    /// pastor had seen the work, but only in the memory of an actor that was
    /// replaced (a daemon restart, a flock or settings reload) before the
    /// agent went idle. What it saw is kept with the task now, so the new
    /// actor settles the idle agent as done.
    #[tokio::test(start_paused = true)]
    async fn work_seen_before_a_restart_still_completes_the_task() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        assert_eq!(h.shutdown().await, ShutdownOutcome::Finished);

        // The agent finishes while no actor watches it.
        fake.set_status_silently(&pane, AgentStatus::Idle);
        let (_h2, _events) = connected(&fake, &store).await;
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// Only work seen after the prompt counts across a restart, as it does
    /// within one actor: an agent never seen at work stays `running`.
    #[tokio::test(start_paused = true)]
    async fn an_agent_never_seen_at_work_stays_running_after_a_restart() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        assert_eq!(h.shutdown().await, ShutdownOutcome::Finished);
        fake.set_status_silently(&pane, AgentStatus::Unknown);
        fake.set_status_silently(&pane, AgentStatus::Idle);
        let (_h2, _events) = connected(&fake, &store).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// herdr's `done` status (idle, not yet looked at) finishes a task the same
    /// way `idle` does.
    #[tokio::test(start_paused = true)]
    async fn an_agent_reporting_done_is_done_after_settle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Done);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// An idle event carries no sequence, so the first settle check cannot tell
    /// whether the agent worked again, unseen, inside the window. It must not
    /// mark the task done: it records the sequence `agent.list` shows and starts
    /// a new window from there. A moved sequence restarts the window again; an
    /// unchanged one at the next check is done.
    #[tokio::test(start_paused = true)]
    async fn an_idle_event_without_a_sequence_waits_a_window_from_the_listed_one() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                // Keep reconcile out of the way: only settle checks list agents.
                reconcile_every: Duration::from_secs(60),
                ..settings_with_settle(Duration::from_millis(400))
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let lists = || {
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.list")
                .count()
        };
        let before = lists();

        // Idle event, no sequence; the agent's sequence is past the baseline.
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("first settle check", || lists() > before).await;
        // The check is logged when it arrives; give it time to act.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "an unknown sequence starts a window, it does not finish one"
        );

        // Worked and went idle again inside the new window, unseen by events.
        fake.set_status_silently(&pane, AgentStatus::Working);
        fake.set_status_silently(&pane, AgentStatus::Idle);
        wait_for("second settle check", || lists() > before + 1).await;
        // The check is logged when it arrives; give it time to act.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "a moved sequence restarts the window"
        );
        assert!(!saw(&mut events, "task.done", t.id));

        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(
            lists() > before + 2,
            "done needs a third check at an unchanged sequence"
        );
    }
    /// An agent that went idle and back to work within `settle` is not done,
    /// even when the flip back to work was missed on the event stream: the
    /// settle check sees `state_change_seq` moved past the value it recorded
    /// and waits another window.
    #[tokio::test(start_paused = true)]
    async fn idle_then_working_again_unseen_within_settle_does_not_complete() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(600);
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                // Reconcile often, so it records the candidate's sequence early.
                reconcile_every: Duration::from_millis(50),
                ..settings_with_settle(settle)
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        // Unseen by events: only reconcile's agent.list notices the idle agent
        // and records the sequence it saw.
        fake.set_status_silently(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Back to work and idle again before the window ends, still unseen.
        fake.set_status_silently(&pane, AgentStatus::Working);
        fake.set_status_silently(&pane, AgentStatus::Idle);
        // Past the first window, well inside the second.
        tokio::time::sleep(Duration::from_millis(550)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "a new state change inside settle restarts the window"
        );
        assert!(!saw(&mut events, "task.done", t.id));
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// An agent that is idle without ever having worked on the prompt has not
    /// done anything: `state_change_seq` never moved past the value recorded
    /// when the prompt was sent, so no amount of settling makes it done.
    #[tokio::test(start_paused = true)]
    async fn an_agent_idle_since_its_prompt_never_completes() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(fake.agents()[0].agent_status, AgentStatus::Idle);
        // An idle event with no state change behind it (herdr sends one when a
        // human looks at a `done` pane) is a candidate, not a completion.
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        // Several settle windows (100ms) and reconciles (200ms). The prompt
        // is sent again meanwhile, and then the task is blocked; never done.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_ne!(state_of(&store, t.id), TaskState::Done);
        assert!(!saw(&mut events, "task.done", t.id));
    }

    /// Count of `agent.list` requests the fake has answered so far.
    fn lists(fake: &FakeHerdr) -> usize {
        fake.requests()
            .iter()
            .filter(|r| r.method == "agent.list")
            .count()
    }

    /// herdr stamps a new `state_change_seq` on `unknown` too, so an agent
    /// that flickers `idle -> unknown -> idle` sits idle past its baseline
    /// without having worked. With no `working` or `blocked` seen since the
    /// prompt, no amount of settling makes that done.
    #[tokio::test(start_paused = true)]
    async fn idle_unknown_idle_without_activity_never_completes() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let baseline = t.last_completion_seq.unwrap();
        fake.set_status(&pane, AgentStatus::Unknown);
        fake.set_status(&pane, AgentStatus::Idle);
        assert!(fake.agents()[0].state_change_seq > baseline);
        // Several settle windows (100ms) and reconciles (200ms).
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(!saw(&mut events, "task.done", t.id));
    }

    /// A working spell only `agent.list` saw (the event was missed) counts as
    /// activity: the later idle completes the task after settle.
    #[tokio::test(start_paused = true)]
    async fn working_seen_only_by_reconcile_counts_as_activity() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status_silently(&pane, AgentStatus::Working);
        // Two lists: the first may have been in flight before the change.
        let before = lists(&fake);
        wait_for("a reconcile saw it working", || lists(&fake) > before + 1).await;
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// `blocked` after the prompt is activity too: the agent asked a human,
    /// got its answer and finished, so its idle completes the task.
    #[tokio::test(start_paused = true)]
    async fn blocked_then_idle_is_done_after_settle() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        // Its screen shows it answered the prompt (`prompt_on_screen`).
        fake.set_pane_text(&pane, "\u{25cf} Done, after your answer.\n");
        fake.set_status_silently(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// A task whose prompt herdr refused (blocked at startup) is not done when
    /// its agent goes idle while the prompt still cannot be delivered, even
    /// though the agent's `state_change_seq` has moved: the moves were its
    /// launch and its startup question, not our work.
    #[tokio::test(start_paused = true)]
    async fn an_agent_idle_before_its_prompt_is_delivered_never_completes() {
        let fake = FakeHerdr::new();
        // Still launching for the whole test: every delivery is `agent_not_ready`.
        fake.set_ready_after(Duration::from_secs(10));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake
            .workspace_create(None, &name, &Default::default())
            .await
            .unwrap();
        let pane = created.root_pane.pane_id.clone();
        fake.agent_start(&name, "claude", &pane, &[]).await.unwrap();
        fake.set_status(&pane, AgentStatus::Blocked);
        t.state = TaskState::Blocked;
        t.prompt_pending = true;
        t.machine = Some("m".into());
        t.pane_id = Some(pane.clone());
        t.agent_name = Some(name);
        store.update_task(&mut t).unwrap();
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        // The human answers; the agent is idle but herdr will not take input yet.
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(700)).await;
        let got = store.get_task(t.id).unwrap().unwrap();
        assert!(got.prompt_pending, "the prompt was never delivered");
        assert_ne!(got.state, TaskState::Done);
        assert!(!saw(&mut events, "task.done", t.id));
    }

    /// Rows left `running` by earlier builds have no baseline. Their agents
    /// did work (`state_change_seq` is past 0) and sit idle, so after this fix
    /// reconcile finishes them instead of holding their slots forever.
    #[tokio::test(start_paused = true)]
    async fn a_running_task_without_a_baseline_completes_when_its_agent_is_idle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake
            .workspace_create(None, &name, &Default::default())
            .await
            .unwrap();
        let pane = created.root_pane.pane_id.clone();
        fake.agent_start(&name, "claude", &pane, &[]).await.unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_status(&pane, AgentStatus::Done);
        t.state = TaskState::Running;
        t.machine = Some("m".into());
        t.pane_id = Some(pane);
        t.agent_name = Some(name);
        store.update_task(&mut t).unwrap();
        assert_eq!(t.last_completion_seq, None);
        let (_h, _events) = spawn(&fake, &store);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_marks_missing_agents_failed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        t.state = TaskState::Running;
        t.machine = Some("m".into());
        t.pane_id = Some("w9:p1".into());
        t.agent_name = Some("t-1".into());
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        assert!(
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .error
                .unwrap()
                .contains("not found on machine")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_closes_done_tasks_whose_pane_vanished() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        t.pane_id = Some("w9:p1".into());
        t.agent_name = Some("t-1".into());
        t.last_completion_seq = Some(1);
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        assert!(
            store.get_task(t.id).unwrap().unwrap().error.is_none(),
            "a done task whose pane vanished did not fail; it just finished"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_adopts_live_agent_state() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let created = fake
            .workspace_create(None, "t-1", &Default::default())
            .await
            .unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Blocked);
        let mut t = new_task(&store);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        t.pane_id = Some(created.root_pane.pane_id.clone());
        t.agent_name = Some("t-1".into());
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
    }

    /// A `Starting` task with no pane recorded is what `run_dispatch` persists right
    /// before its first herdr call (see the comment there); a crash between that
    /// write and the pane/workspace ids being recorded leaves exactly this row.
    /// `reconcile` must find the agent dispatch would have started (named `t-<id>`)
    /// and adopt it instead of leaving the task stuck, or worse, re-dispatching it.
    /// The row here has no `agent_name` either, and the fake agent is left idle (no
    /// `agent.prompt`): both are deliberately the plainest shape a crash can leave
    /// (`agent_name` is now always set by `run_dispatch`'s pre-persist, but adoption
    /// must not *depend* on that; a crash between `agent.start` and `agent.prompt`
    /// is exactly as real), so this exercises adoption doing all the work itself,
    /// not a test fixture that already looks like a successful dispatch.
    #[tokio::test(start_paused = true)]
    async fn reconcile_adopts_a_starting_task_by_agent_name() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake
            .workspace_create(None, &name, &Default::default())
            .await
            .unwrap();
        fake.agent_start(&name, "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        // pane_id, workspace_id and agent_name are all left unset.
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(500)),
        );
        wait_for("adopted and running", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        let got = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            got.pane_id.as_deref(),
            Some(created.root_pane.pane_id.as_str())
        );
        assert_eq!(
            got.workspace_id.as_deref(),
            Some(created.workspace.workspace_id.as_str())
        );
        assert_eq!(
            got.agent_name.as_deref(),
            Some(name.as_str()),
            "adoption must set agent_name itself, not rely on it already being there"
        );

        // Idle with no completed work must not fast-track to Done, and work
        // finished later must still wait out the settle window, same as the
        // pane-known path (mirrors `dispatch_then_events_drive_state`).
        // Adopted panes get no status events until the next reconnect, so
        // the working spell counts once a reconcile has listed it.
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Working);
        let before = lists(&fake);
        wait_for("a reconcile saw it working", || lists(&fake) > before + 1).await;
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "done waits for the settle window"
        );
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// A crash can leave a `Starting` row whose agent is still launching:
    /// `agent.list` shows it `launch_pending`, so dispatch never sent it the
    /// prompt. Adoption must keep that delivery pending. When the launch ends
    /// the agent goes idle at a newer `state_change_seq` than the one adopted;
    /// that is its startup, not this task's work, so the task is not done:
    /// the prompt is sent then, and only idle past the prompt reply's sequence
    /// completes it.
    #[tokio::test(start_paused = true)]
    async fn reconcile_keeps_the_prompt_pending_for_a_launching_agent() {
        let fake = FakeHerdr::new();
        let ready_after = Duration::from_millis(600);
        fake.set_ready_after(ready_after);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake
            .workspace_create(None, &name, &Default::default())
            .await
            .unwrap();
        let pane = created.root_pane.pane_id.clone();
        fake.agent_start(&name, "claude", &pane, &[]).await.unwrap();
        let launched = Instant::now();
        // As herdr has it: nothing detected while launching, so the end of the
        // launch (unknown -> idle) is a state change with a new sequence.
        fake.set_status_silently(&pane, AgentStatus::Unknown);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        store.update_task(&mut t).unwrap();
        let (_h, mut events) = spawn(&fake, &store);
        let prompts = || {
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.prompt")
                .count()
        };

        wait_for("adopted", || {
            store.get_task(t.id).unwrap().unwrap().pane_id.is_some()
        })
        .await;
        assert!(
            launched.elapsed() < ready_after,
            "adopted while the agent was still launching"
        );
        assert!(
            store.get_task(t.id).unwrap().unwrap().prompt_pending,
            "a launch-pending agent has not seen the prompt"
        );

        tokio::time::sleep(ready_after.saturating_sub(launched.elapsed())).await;
        assert_eq!(prompts(), 0, "nothing is sent before the agent is ready");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(prompts(), 1, "the prompt is sent once, after the launch");
        let got = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            got.last_completion_seq,
            Some(fake.agents()[0].state_change_seq),
            "the baseline is the prompt reply's sequence"
        );

        // Well past settle and a reconcile: the agent is working, not done.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(!saw(&mut events, "task.done", t.id));

        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// Same interrupted-dispatch shape, but no agent named `t-<id>` exists anywhere:
    /// the crash happened before `agent.start` even ran. Nothing to adopt, so the
    /// task must fail, not hang forever as `Starting`.
    #[tokio::test(start_paused = true)]
    async fn reconcile_fails_a_starting_task_with_no_agent() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        // agent_name is left unset: `reconcile` matches by the name it derives from
        // the task id, not by reading a persisted `agent_name` back.
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        assert!(
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .error
                .unwrap()
                .contains("interrupted")
        );
    }

    struct Refusing;
    impl Connector for Refusing {
        fn connect(&self) -> ConnectFuture<'_> {
            Box::pin(async {
                Err(ConnectError {
                    message: "ssh: permission denied (255)".into(),
                })
            })
        }
        fn describe(&self) -> String {
            "refusing".into()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn machine_reports_reconnecting_with_error() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, mut rx) = broadcast::channel(64);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(Refusing),
            store,
            settings(),
            events,
        );
        wait_for("reconnecting", || {
            h.snapshot().channel == ChannelState::Reconnecting
        })
        .await;
        assert!(h.snapshot().error.unwrap().contains("permission denied"));
        let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.kind, "machine.lost");
        assert_eq!(h.snapshot().live, 0);
        // With a 50ms initial backoff (200ms max) there are many more connect
        // attempts within the next second; `machine.lost` must not repeat since
        // the machine never recovers.
        let extra = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await;
        assert!(extra.is_err(), "machine.lost repeated: {extra:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_triggers_reconnect_and_resubscribe() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        fake.disconnect_all();
        // The reconnecting state is too brief to observe reliably; the events prove
        // the cycle instead. A disconnect after being connected always announces
        // the outage before the recovery (see `announce_lost`/`announce_connected`),
        // so a break followed by a successful reconnect emits a matched
        // `machine.lost` / `machine.connected` pair, never `connected` alone.
        let mut saw_lost = false;
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("machine.connected within 5s")
                .unwrap();
            if ev.kind == "machine.lost" {
                saw_lost = true;
            } else if ev.kind == "machine.connected" {
                assert!(
                    saw_lost,
                    "machine.connected without a preceding machine.lost"
                );
                break;
            }
        }
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        wait_for("blocked after resubscribe", || {
            state_of(&store, t.id) == TaskState::Blocked
        })
        .await;
    }

    /// A CLI talks to whatever head is running, which may predate the field.
    #[test]
    fn a_status_from_an_older_head_reads_without_a_pastor_version() {
        let s: MachineStatus = serde_json::from_value(serde_json::json!({
            "name": "pi-3", "host": "fleet@pi-3", "endpoint": "ssh fleet@pi-3",
            "channel": "connected", "herdr_version": "0.9.1", "protocol": 22,
            "error": null, "live": 0, "max_agents": 2, "tags": []
        }))
        .unwrap();
        assert_eq!(s.pastor_version, None);
    }

    /// Each connect asks the machine for its pastor version once, and the
    /// status carries the answer until the next connect asks again.
    #[tokio::test(start_paused = true)]
    async fn status_carries_the_pastor_version_from_each_connect() {
        let fake = FakeHerdr::new();
        fake.set_pastor_version(Some("0.2.0"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        assert_eq!(h.snapshot().pastor_version.as_deref(), Some("0.2.0"));

        fake.set_pastor_version(None);
        fake.disconnect_all();
        wait_for("unknown after a reconnect", || {
            h.snapshot().pastor_version.is_none() && h.snapshot().channel == ChannelState::Connected
        })
        .await;
    }

    /// An upgrade on a machine that stays connected shows without a restart
    /// or a reconnect: the reconcile tick asks again once `version_every`
    /// has passed.
    #[tokio::test(start_paused = true)]
    async fn a_connected_machine_picks_up_a_new_pastor_version() {
        let fake = FakeHerdr::new();
        fake.set_pastor_version(Some("0.2.0"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        assert_eq!(h.snapshot().pastor_version.as_deref(), Some("0.2.0"));
        // `machine describe` says when the version was last read, so a
        // fresh install that still shows the old one can be told from a
        // head that never asked.
        h.snapshot().pastor_read_at.expect("read at connect");
        // Age the read time by hand: the paused clock barely moves, so the
        // refresh has to be seen to write a newer one.
        let old = Utc::now() - chrono::Duration::hours(1);
        h.status.write().recover().pastor_read_at = Some(old);

        fake.set_pastor_version(Some("0.3.0"));
        wait_for("the new version", || {
            h.snapshot().pastor_version.as_deref() == Some("0.3.0")
        })
        .await;
        let pings = fake
            .requests()
            .iter()
            .filter(|r| r.method == "ping")
            .count();
        assert_eq!(pings, 1, "picked up without a reconnect");
        assert!(h.snapshot().pastor_read_at.expect("read again") > old);
    }

    /// A machine with pins reports the version of each pinned harness and
    /// whether it drifts; the reading follows an upgrade without a reconnect,
    /// a CLI that is not there is not drift, and a machine with no pins
    /// reports nothing.
    #[tokio::test(start_paused = true)]
    async fn status_carries_harness_drift_against_the_pins() {
        let fake = FakeHerdr::new();
        fake.set_harness_version("claude", Some("2.1.309"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pins = |kv: &[(&str, &str)]| -> MachineSettings {
            MachineSettings {
                harness_pins: kv
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                ..settings()
            }
        };
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            pins(&[("claude", "2.1.310"), ("agy", "1.4")]),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let got = |h: &MachineHandle| -> Vec<(String, Option<String>, bool)> {
            h.snapshot()
                .harness
                .iter()
                .map(|s| (s.kind.clone(), s.version.clone(), s.drift))
                .collect()
        };
        // Sorted by kind; agy is not installed, which is not drift.
        assert_eq!(
            got(&h),
            [
                ("agy".to_string(), None, false),
                ("claude".to_string(), Some("2.1.309".into()), true)
            ]
        );
        // An upgrade to the pin clears it at the next refresh.
        fake.set_harness_version("claude", Some("2.1.310"));
        wait_for("no drift", || h.snapshot().harness.iter().all(|s| !s.drift)).await;
        assert_eq!(h.snapshot().harness[1].version.as_deref(), Some("2.1.310"));

        let (plain, _e) = spawn(&fake, &store);
        wait_for("connected", || {
            plain.snapshot().channel == ChannelState::Connected
        })
        .await;
        assert!(plain.snapshot().harness.is_empty());
    }

    /// A reconnect whose version ask fails (the ping worked, the extra ssh
    /// did not) keeps the version last read and when it was read, as the
    /// periodic refresh does: a blank would read as "no pastor there".
    #[tokio::test(start_paused = true)]
    async fn a_reconnect_whose_version_ask_fails_keeps_the_last_read() {
        let fake = FakeHerdr::new();
        fake.set_pastor_version(Some("0.2.0"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let first = h.snapshot().pastor_read_at.expect("read at connect");

        fake.set_pastor_version_fails(true);
        fake.disconnect_all();
        wait_for("reconnected", || {
            let pings = fake
                .requests()
                .iter()
                .filter(|r| r.method == "ping")
                .count();
            pings >= 2 && h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let s = h.snapshot();
        assert_eq!(s.pastor_version.as_deref(), Some("0.2.0"));
        assert_eq!(s.pastor_read_at, Some(first));
    }

    /// The same refresh while polling: a machine whose events will not open
    /// still answers requests and can stay that way for as long as it likes.
    #[tokio::test(start_paused = true)]
    async fn a_polling_machine_picks_up_a_new_pastor_version() {
        let fake = FakeHerdr::new();
        fake.set_pastor_version(Some("0.2.0"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let mut settings = settings();
        settings.poll_every = Duration::from_millis(50);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                // The first subscribe fails fast, so the machine polls at
                // once; every retry wedges, so it keeps polling.
                fail_until: 1,
                wedge_forever: true,
                slow_ack: None,
                fake: fake.clone(),
            }),
            store,
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        assert_eq!(h.snapshot().pastor_version.as_deref(), Some("0.2.0"));

        fake.set_pastor_version(Some("0.3.0"));
        wait_for("the new version", || {
            h.snapshot().pastor_version.as_deref() == Some("0.3.0")
        })
        .await;
        assert_eq!(h.snapshot().channel, ChannelState::Polling);
    }

    /// A CLI talks to whatever head is running, which may predate the room.
    #[test]
    fn a_status_from_an_older_head_reads_without_a_room() {
        let s: MachineStatus = serde_json::from_value(serde_json::json!({
            "name": "pi-3", "host": "fleet@pi-3", "endpoint": "ssh fleet@pi-3",
            "channel": "connected", "herdr_version": "0.9.1", "protocol": 22,
            "error": null, "live": 0, "max_agents": 2, "tags": []
        }))
        .unwrap();
        assert_eq!(s.room, None);
        let r: Room = serde_json::from_value(serde_json::json!({
            "home_free": 10, "tmp_free": null, "mem_free": 30
        }))
        .unwrap();
        assert_eq!(r.home_free, Some(10));
        assert_eq!(r.asked_at, None);
        assert_eq!(r.error, None);
    }

    fn room(home: u64, tmp: u64, mem: u64) -> Room {
        Room {
            home_free: Some(home),
            tmp_free: Some(tmp),
            mem_free: Some(mem),
            asked_at: None,
            error: None,
        }
    }

    fn room_of(h: &MachineHandle) -> Room {
        h.snapshot().room.expect("a room")
    }

    /// Each connect reads the machine's room, and the status carries the
    /// numbers with when they were read and no error.
    #[tokio::test(start_paused = true)]
    async fn status_carries_the_room_from_connect() {
        let fake = FakeHerdr::new();
        fake.set_room(Some(room(100, 20, 30)));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let r = room_of(&h);
        assert_eq!(
            (r.home_free, r.tmp_free, r.mem_free),
            (Some(100), Some(20), Some(30))
        );
        r.asked_at.expect("read at connect");
        assert_eq!(r.error, None);
    }

    /// A machine that cannot say (a `command` machine, or macOS for
    /// memory) carries what it could say and nothing for the rest.
    #[tokio::test(start_paused = true)]
    async fn a_machine_that_cannot_say_has_no_room() {
        let fake = FakeHerdr::new();
        fake.set_room(None);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        assert_eq!(h.snapshot().room, None);
        fake.set_room(Some(Room {
            mem_free: None,
            ..room(100, 20, 0)
        }));
        wait_for("a room without memory", || {
            h.snapshot().room.is_some_and(|r| r.home_free == Some(100))
        })
        .await;
        assert_eq!(room_of(&h).mem_free, None);
    }

    /// The room is read again every `room_every` while the machine stays
    /// connected: the numbers move with the disk, without a reconnect.
    #[tokio::test(start_paused = true)]
    async fn a_connected_machine_refreshes_its_room() {
        let fake = FakeHerdr::new();
        fake.set_room(Some(room(100, 20, 30)));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let old = Utc::now() - chrono::Duration::hours(1);
        h.status.write().recover().room.as_mut().unwrap().asked_at = Some(old);

        fake.set_room(Some(room(50, 20, 30)));
        wait_for("the new numbers", || {
            h.snapshot().room.is_some_and(|r| r.home_free == Some(50))
        })
        .await;
        let pings = fake
            .requests()
            .iter()
            .filter(|r| r.method == "ping")
            .count();
        assert_eq!(pings, 1, "picked up without a reconnect");
        assert!(room_of(&h).asked_at.expect("read again") > old);
    }

    /// A probe that fails keeps the numbers last read and when they were
    /// read, and says what went wrong; the next one that answers clears
    /// it. The channel never moves over a room: the ping and reconcile just
    /// proved the machine reachable.
    #[tokio::test(start_paused = true)]
    async fn a_failed_room_probe_keeps_the_numbers_and_sets_the_error() {
        let fake = FakeHerdr::new();
        fake.set_room(Some(room(100, 20, 30)));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let first = room_of(&h).asked_at.expect("read at connect");

        fake.set_room_fails(true);
        wait_for("the error", || {
            h.snapshot().room.is_some_and(|r| r.error.is_some())
        })
        .await;
        let r = room_of(&h);
        assert_eq!(
            (r.home_free, r.tmp_free, r.mem_free),
            (Some(100), Some(20), Some(30))
        );
        assert_eq!(r.asked_at, Some(first), "the numbers keep their age");
        assert!(
            r.error.as_deref().unwrap().contains("connection reset"),
            "{r:?}"
        );
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|r| r.method == "ping")
                .count(),
            1,
            "no reconnect over a room"
        );

        fake.set_room_fails(false);
        fake.set_room(Some(room(90, 20, 30)));
        wait_for("the error cleared", || {
            h.snapshot()
                .room
                .is_some_and(|r| r.error.is_none() && r.home_free == Some(90))
        })
        .await;
        assert!(room_of(&h).asked_at.unwrap() > first);
    }

    /// A reconnect whose room probe fails keeps the room last read too, with
    /// the error, rather than starting from nothing.
    #[tokio::test(start_paused = true)]
    async fn a_reconnect_whose_room_probe_fails_keeps_the_last_read() {
        let fake = FakeHerdr::new();
        fake.set_room(Some(room(100, 20, 30)));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let first = room_of(&h).asked_at.expect("read at connect");

        fake.set_room_fails(true);
        fake.disconnect_all();
        wait_for("reconnected", || {
            let pings = fake
                .requests()
                .iter()
                .filter(|r| r.method == "ping")
                .count();
            pings >= 2 && h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let r = room_of(&h);
        assert_eq!(r.home_free, Some(100));
        assert_eq!(r.asked_at, Some(first));
        assert!(r.error.is_some(), "{r:?}");
    }

    /// A first probe that fails still carries the error, with no numbers.
    #[tokio::test(start_paused = true)]
    async fn a_first_room_probe_that_fails_carries_only_the_error() {
        let fake = FakeHerdr::new();
        fake.set_room_fails(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let r = room_of(&h);
        assert_eq!((r.home_free, r.tmp_free, r.mem_free), (None, None, None));
        assert_eq!(r.asked_at, None);
        assert!(r.error.is_some(), "{r:?}");
    }

    /// The same refresh while polling: a machine whose events will not open
    /// still answers requests and can stay that way for as long as it likes.
    #[tokio::test(start_paused = true)]
    async fn a_polling_machine_refreshes_its_room() {
        let fake = FakeHerdr::new();
        fake.set_room(Some(room(100, 20, 30)));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let mut settings = settings();
        settings.poll_every = Duration::from_millis(50);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail_until: 1,
                wedge_forever: true,
                slow_ack: None,
                fake: fake.clone(),
            }),
            store,
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        assert_eq!(room_of(&h).home_free, Some(100));

        fake.set_room(Some(room(50, 20, 30)));
        wait_for("the new numbers", || {
            h.snapshot().room.is_some_and(|r| r.home_free == Some(50))
        })
        .await;
        assert_eq!(h.snapshot().channel, ChannelState::Polling);
    }

    /// Sixty seconds by default: a number that moves with every checkout
    /// and build, read often enough to place on, at one probe per machine
    /// per minute.
    #[test]
    fn room_every_is_a_minute_by_default() {
        assert_eq!(
            MachineSettings::default().room_every,
            Duration::from_secs(60)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn incompatible_protocol_is_reported_and_not_dispatched_to() {
        let fake = FakeHerdr::new();
        fake.set_protocol(20);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("incompatible", || {
            h.snapshot().channel == ChannelState::Incompatible
        })
        .await;
        assert_eq!(h.snapshot().protocol, Some(20));
        let err = h.dispatch(new_task(&store).id).await.unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn stale_after_timeout() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    timeout_secs: 1,
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap();
        let t = h.dispatch(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        // The timeout is calendar time, which no paused clock moves.
        start_an_hour_ago(&store, t.id);
        wait_for("stale", || state_of(&store, t.id) == TaskState::Stale).await;
        assert_eq!(fake.agents().len(), 1, "nothing was killed");
    }

    /// A task with a one-second timeout, as `stale_after_timeout` makes.
    fn new_task_timing_out(store: &Store) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    timeout_secs: 1,
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    /// Moves a task's start an hour back, well past its timeout, retrying
    /// when the actor wrote the row in between.
    fn start_an_hour_ago(store: &Store, id: i64) {
        loop {
            let mut t = store.get_task(id).unwrap().unwrap();
            t.started_at = Some(Utc::now() - chrono::Duration::hours(1));
            match store.update_task(&mut t) {
                Ok(()) => return,
                Err(err) if err.downcast_ref::<crate::store::Conflict>().is_some() => {}
                Err(err) => panic!("{err:#}"),
            }
        }
    }

    /// The timeout counts from the latest start: a done task given more to
    /// do after its timeout runs again, and is not stale on the next
    /// reconcile.
    #[tokio::test(start_paused = true)]
    async fn a_done_task_sent_more_work_after_its_timeout_runs() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_timing_out(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        start_an_hour_ago(&store, t.id);
        let before = Utc::now();
        let sent = h
            .send(
                t.id,
                SendInput {
                    text: Some("one more thing".into()),
                    enter: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(sent.state, TaskState::Running);
        assert!(sent.started_at.unwrap() >= before, "the reopen restarts it");
        fake.set_status(&pane, AgentStatus::Working);
        // Some reconciles later, within the new timeout.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// A paused task never goes stale, however long it waits, and a resumed
    /// one counts its timeout from the resume.
    #[tokio::test(start_paused = true)]
    async fn a_paused_task_is_never_stale_and_a_resumed_one_counts_from_its_resume() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let mut t = new_task_timing_out(&store);
        t.state = TaskState::Paused;
        t.machine = Some("m".into());
        t.spec.session_id = crate::task::new_session_id();
        t.started_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.update_task(&mut t).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Paused);
        let before = Utc::now();
        let t = h.resume(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert!(t.started_at.unwrap() >= before, "the resume restarts it");
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Working);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        start_an_hour_ago(&store, t.id);
        wait_for("stale", || state_of(&store, t.id) == TaskState::Stale).await;
    }

    /// A blocked task waits for a person, not for its agent: past its
    /// timeout it stays blocked.
    #[tokio::test(start_paused = true)]
    async fn a_blocked_task_past_its_timeout_stays_blocked() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_timing_out(&store).id).await.unwrap();
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        start_an_hour_ago(&store, t.id);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
    }

    /// A block cleared past the old deadline gets a fresh timeout: the time
    /// spent waiting for a person does not count against it.
    #[tokio::test(start_paused = true)]
    async fn a_block_cleared_past_its_timeout_gets_fresh_time() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_timing_out(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        start_an_hour_ago(&store, t.id);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Blocked,
            "still waiting on a person"
        );
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;
        // Some reconciles later, within the new timeout, not the old one.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    /// A running task past its timeout that a reconcile is the first to see
    /// blocked (its event was missed) is blocked, not stale.
    #[tokio::test(start_paused = true)]
    async fn a_task_first_listed_blocked_past_its_timeout_is_blocked() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_timing_out(&store).id).await.unwrap();
        // Blocked first: a reconcile between the two would see a running task
        // past its timeout that is not blocked, which is stale.
        fake.set_status_silently(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        start_an_hour_ago(&store, t.id);
        wait_for("blocked", || state_of(&store, t.id) != TaskState::Running).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
    }

    /// A stale task whose agent finishes after all is done, through the
    /// actor, and `task.stale` went out once, not on every reconcile.
    #[tokio::test(start_paused = true)]
    async fn a_stale_task_whose_agent_completes_is_done() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_timing_out(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        start_an_hour_ago(&store, t.id);
        wait_for("stale", || state_of(&store, t.id) == TaskState::Stale).await;
        // Some reconciles while stale.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Stale);
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        let mut stale = 0;
        loop {
            match events.try_recv() {
                Ok(ev) if ev.kind == "task.stale" => stale += 1,
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(err) => panic!("{err}"),
            }
        }
        assert_eq!(stale, 1);
    }

    /// Every request is served by the fake except `events.subscribe`, which gets
    /// a connection that closes without answering until `subscribes` reaches
    /// `fail_until`. Selecting by method, not by call parity: a connect attempt
    /// makes several ordinary calls (ping, reconcile's `agent.list`) before it
    /// subscribes, so a parity rule would fail one of those instead and never
    /// reach `open_events` at all.
    ///
    /// Past `fail_until`, `wedge_forever` chooses what "past the failures"
    /// means: `false` (most tests) hands the subscription to the wrapped fake
    /// for real; `true` accepts the connection and then never acknowledges the
    /// subscribe at all — the shape of a herdr that is permanently wedged, not
    /// just failing, used to prove a stuck subscribe cannot block the poll loop.
    ///
    /// `slow_ack`, independent of `wedge_forever`, adds a fixed delay before
    /// acking and proxying to the fake: long enough for a test to act (e.g.
    /// dispatch a task) while this attempt is still in flight but not wedged
    /// forever, to prove a *later* attempt (built from the pane set as it
    /// stands then) is the one that actually lands the subscription.
    struct FlakyEvents {
        subscribes: Arc<std::sync::atomic::AtomicUsize>,
        fail_until: usize,
        wedge_forever: bool,
        slow_ack: Option<Duration>,
        fake: FakeHerdr,
    }

    impl Connector for FlakyEvents {
        fn connect(&self) -> ConnectFuture<'_> {
            let fake = self.fake.clone();
            let subscribes = self.subscribes.clone();
            let fail_until = self.fail_until;
            let wedge_forever = self.wedge_forever;
            let slow_ack = self.slow_ack;
            Box::pin(async move {
                let (a, b) = tokio::io::duplex(64 * 1024);
                let (ar, aw) = tokio::io::split(a);
                let (br, bw) = tokio::io::split(b);
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                    let mut reader = tokio::io::BufReader::new(br);
                    let mut writer = bw;
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let Ok(req) = serde_json::from_str::<crate::herdr::Request>(line.trim()) else {
                        return;
                    };
                    if req.method == "events.subscribe" {
                        let n = subscribes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if n < fail_until {
                            return; // dropping `writer` is the EOF the subscribe sees
                        }
                        if wedge_forever {
                            // Accept the connection but never write a reply: no
                            // EOF, no ack, nothing. Held here for the lifetime of
                            // the test.
                            std::future::pending::<()>().await;
                        }
                        if let Some(delay) = slow_ack {
                            tokio::time::sleep(delay).await;
                        }
                        // Past the failures (and any delay): hand the subscription to the fake for real.
                        let mut stream = match fake
                            .subscribe(
                                req.params
                                    .get("subscriptions")
                                    .and_then(|v| v.as_array())
                                    .cloned()
                                    .unwrap_or_default(),
                            )
                            .await
                        {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                        let ack = serde_json::json!({"id": req.id, "result": {"type": "subscription_started"}});
                        if writer
                            .write_all(format!("{ack}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        while let Ok(ev) = stream.next().await {
                            let line = serde_json::to_string(&ev).unwrap();
                            if writer
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        return;
                    }
                    // Anything else is relayed to the fake on a connection of its
                    // own and the reply is handed back with the caller's id.
                    let reply = match fake.connect().call(&req.method, req.params).await {
                        Ok(result) => serde_json::json!({"id": req.id, "result": result}),
                        Err(crate::herdr::HerdrError::Api { code, message }) => {
                            serde_json::json!({"id": req.id, "error": {"code": code, "message": message}})
                        }
                        Err(_) => return,
                    };
                    let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
                });
                Ok(Connection::new(Box::new(ar), Box::new(aw)))
            })
        }
        fn describe(&self) -> String {
            "flaky events".into()
        }
        fn pastor_version(&self) -> crate::herdr::transport::VersionFuture<'_> {
            self.fake.pastor_version()
        }
        fn room(&self) -> crate::herdr::transport::RoomFuture<'_> {
            self.fake.room()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn event_stream_failures_back_off() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connector = FlakyEvents {
            subscribes: subscribes.clone(),
            fail_until: usize::MAX,
            wedge_forever: false,
            slow_ack: None,
            fake,
        };
        let _h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(connector),
            store,
            settings(),
            events,
        );
        // Busy-spinning would run this thousands of times in 300ms; exponential
        // backoff (50ms, 100ms, 200ms, 200ms, ...) keeps it to about 3 attempts
        // in that window. Bound above that to catch a real spin, and from
        // below, so a test that stopped reaching the subscribe at all fails too.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let n = subscribes.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (2..12).contains(&n),
            "events.subscribe attempted {n} times in 300ms: expected a few, backing off"
        );
    }

    /// Requests work (ping, agent.list) but the event subscription will not open:
    /// the `polling` state. The machine stays dispatchable, tasks are
    /// tracked by reconcile every `poll_every`, no `machine.lost` is announced,
    /// and the first successful subscribe returns it to `connected`.
    ///
    /// `fail_until: 6` keeps the subscribe failing past the local retry delay's
    /// climb from `initial_backoff` (50ms) to `max_backoff` (200ms, both from
    /// `settings()`): attempts land at roughly 0 (the initial one, in `run`,
    /// before `Polling`), 50, 150, 350, 550, 750ms, and the 7th, at ~950ms,
    /// succeeds. That is comfortably past the 100ms `poll_every` below, so the
    /// Blocked status set right after dispatch is certain to be caught by a
    /// poll tick while still `Polling`, not by a lucky `Connected` reconcile.
    #[tokio::test(start_paused = true)]
    async fn a_machine_whose_events_will_not_open_polls_instead_of_dropping() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, mut rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.poll_every = Duration::from_millis(100);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: subscribes.clone(),
                fail_until: 6,
                wedge_forever: false,
                slow_ack: None,
                fake: fake.clone(),
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        assert!(
            h.snapshot()
                .error
                .as_deref()
                .unwrap_or("")
                .contains("events"),
            "{:?}",
            h.snapshot().error
        );

        // Dispatch works while polling.
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        // Without an event stream, only the poll can see this change.
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        wait_for("blocked via poll", || {
            state_of(&store, t.id) == TaskState::Blocked
        })
        .await;
        assert_eq!(
            h.snapshot().channel,
            ChannelState::Polling,
            "the change must have been caught by the poll tick, not a lucky connected reconcile"
        );

        wait_for("connected once the subscribe succeeds", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        // Never lost: the machine answered every request throughout.
        while let Ok(ev) = rx.try_recv() {
            assert_ne!(ev.kind, "machine.lost", "{ev:?}");
        }
    }

    /// A wedged `events.subscribe` (a herdr that accepts the connection and
    /// never acknowledges it, forever, not just for one attempt) must not block
    /// the poll loop: `dispatch` must still reply promptly, and the channel must
    /// stay `Polling`. `request_timeout` is set high (10s) so the subscribe's
    /// own internal timeout cannot rescue a blocking implementation within the
    /// window this test actually waits; the old inline `self.open_events().await`
    /// in the retry arm would have held up commands and poll ticks for up to
    /// that long.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_subscribe_does_not_block_dispatch_while_polling() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.request_timeout = Duration::from_secs(10);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: subscribes.clone(),
                fail_until: 1,
                wedge_forever: true,
                slow_ack: None,
                fake,
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        // Wait until the retry timer has actually fired and the wedged attempt
        // (the one past `fail_until`) is in flight server-side, not just until
        // `Polling` is reached: dispatching too early would race the retry
        // timer and could get served before the wedge even starts, proving
        // nothing.
        wait_for("wedged subscribe attempt in flight", || {
            subscribes.load(std::sync::atomic::Ordering::SeqCst) >= 2
        })
        .await;

        let dispatched =
            tokio::time::timeout(Duration::from_secs(2), h.dispatch(new_task(&store).id))
                .await
                .expect("dispatch must not be blocked by a wedged subscribe attempt");
        assert_eq!(dispatched.unwrap().state, TaskState::Running);
        assert_eq!(
            h.snapshot().channel,
            ChannelState::Polling,
            "a wedged subscribe attempt must not be mistaken for a lost machine"
        );
    }

    /// A dispatch that adds a pane while a subscribe attempt is already in
    /// flight must not let that attempt land the machine on `Connected` with a
    /// stale subscription. The in-flight attempt was built (by
    /// `subscribe_future`) from the pane set as it stood *before* the dispatch;
    /// left alone, it would ack with a subscription missing the new pane, and
    /// that pane's status changes would go unseen until the next
    /// `reconcile_tick` — set to 30s here, far past this test's `wait_for`
    /// bound, so only the event stream delivering the Blocked transition can
    /// make the test pass.
    ///
    /// `slow_ack: 300ms` (not `wedge_forever`) is used so the attempt actually
    /// completes on its own if the resubscribe-on-`Dispatch` fix is missing:
    /// a wedged attempt would just hang forever and this test would time out
    /// either way, proving nothing about *which* subscription landed.
    #[tokio::test(start_paused = true)]
    async fn a_pane_added_mid_subscribe_is_not_lost_to_a_stale_subscription() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.reconcile_every = Duration::from_secs(30);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: subscribes.clone(),
                fail_until: 1,
                wedge_forever: false,
                slow_ack: Some(Duration::from_millis(300)),
                fake: fake.clone(),
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        // The attempt past `fail_until` (its request line has been read
        // server-side, so it is now in its 300ms `slow_ack` sleep) is the one
        // built without the pane the dispatch below is about to add.
        wait_for("subscribe attempt in flight", || {
            subscribes.load(std::sync::atomic::Ordering::SeqCst) >= 2
        })
        .await;

        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);

        wait_for("connected once a subscribe lands", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;

        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        wait_for(
            "blocked via the event stream, not the 30s reconcile",
            || state_of(&store, t.id) == TaskState::Blocked,
        )
        .await;
    }

    /// The reply to a dispatch must carry an already-refreshed live count: a
    /// caller that reads `snapshot().live` the moment `dispatch` returns is the
    /// serialised dispatch pass deciding whether the machine has room left.
    #[tokio::test(start_paused = true)]
    async fn live_count_is_current_when_dispatch_replies() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        for expected in 1..=2 {
            h.dispatch(new_task(&store).id).await.unwrap();
            assert_eq!(h.snapshot().live, expected, "live count lagged the reply");
        }
    }

    #[test]
    fn channel_states_that_accept_dispatch() {
        assert!(ChannelState::Connected.accepts_dispatch());
        assert!(ChannelState::Polling.accepts_dispatch());
        for s in [
            ChannelState::Connecting,
            ChannelState::Reconnecting,
            ChannelState::Incompatible,
        ] {
            assert!(!s.accepts_dispatch(), "{s}");
        }
        assert_eq!(ChannelState::Polling.to_string(), "polling");
    }

    /// Wraps any connector and can be broken: while broken, every connection it
    /// hands out is already at EOF, so the first read of any request fails at
    /// the transport level. That is what a machine that went away looks like
    /// now that no connection is held between requests.
    struct Breakable {
        inner: Arc<dyn Connector>,
        broken: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Connector for Breakable {
        fn connect(&self) -> ConnectFuture<'_> {
            let broken = self.broken.load(std::sync::atomic::Ordering::SeqCst);
            let inner = self.inner.clone();
            Box::pin(async move {
                if broken {
                    Ok(Connection::new(
                        Box::new(tokio::io::empty()),
                        Box::new(tokio::io::sink()),
                    ))
                } else {
                    inner.connect().await
                }
            })
        }
        fn describe(&self) -> String {
            "breakable".into()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_failing_at_the_transport_level_triggers_reconnect() {
        let fake = FakeHerdr::new();
        let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(Breakable {
                inner: Arc::new(fake.clone()),
                broken: broken.clone(),
            }),
            store.clone(),
            settings(),
            events,
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        h.dispatch(new_task(&store).id).await.unwrap();

        broken.store(true, std::sync::atomic::Ordering::SeqCst);
        // The machine is gone: this dispatch must fail rather than hang or
        // silently succeed against a connection that answers nothing.
        let t2 = new_task(&store);
        let err = h.dispatch(t2.id).await;
        assert!(err.is_err(), "dispatch against a dead machine must fail");
        assert_eq!(state_of(&store, t2.id), TaskState::Failed);

        broken.store(false, std::sync::atomic::Ordering::SeqCst);
        wait_for("reconnected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t3 = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t3.state, TaskState::Running);
    }

    /// A request failing at the transport level while `Polling` (not just while
    /// `Connected`) must take the same reconnect path: `Reconnecting`,
    /// `machine.lost`, then back to `Polling` (its subscribe never succeeds
    /// here) once requests work again, with `machine.connected` marking the
    /// recovery. `FlakyEvents` never lets the subscribe through
    /// (`fail_until: usize::MAX`), so the machine can only ever be `Polling` or
    /// `Reconnecting`, never `Connected`, which keeps this test about the
    /// poll loop's own `PollExit::Reconnect` path specifically.
    #[tokio::test(start_paused = true)]
    async fn a_broken_request_while_polling_reconnects_and_returns_to_polling() {
        let fake = FakeHerdr::new();
        let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, mut rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.poll_every = Duration::from_millis(50);
        let inner: Arc<dyn Connector> = Arc::new(FlakyEvents {
            subscribes: subscribes.clone(),
            fail_until: usize::MAX,
            wedge_forever: false,
            slow_ack: None,
            fake,
        });
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(Breakable {
                inner,
                broken: broken.clone(),
            }),
            store,
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;

        broken.store(true, std::sync::atomic::Ordering::SeqCst);
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("machine.lost within 5s")
                .unwrap();
            if ev.kind == "machine.lost" {
                break;
            }
        }
        wait_for("reconnecting", || {
            h.snapshot().channel == ChannelState::Reconnecting
        })
        .await;

        broken.store(false, std::sync::atomic::Ordering::SeqCst);
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("machine.connected within 5s")
                .unwrap();
            if ev.kind == "machine.connected" {
                break;
            }
        }
        assert_eq!(h.snapshot().channel, ChannelState::Polling);
    }

    #[tokio::test(start_paused = true)]
    async fn dispatch_over_a_wedged_connection_times_out_fails_and_reconnects() {
        let fake = FakeHerdr::new();
        // `agent.start` is the first herdr call inside `dispatch()` that can hang;
        // hanging it exercises the timeout without needing the connect/ping/
        // reconcile/subscribe path (none of those are hung) to also cooperate.
        fake.hang_method("agent.start");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut settings = settings();
        settings.request_timeout = Duration::from_millis(100);
        settings.agent_ready_timeout = Duration::from_millis(50);
        let (h, mut events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;

        let t = new_task(&store);
        let err = h.dispatch(t.id).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(state_of(&store, t.id), TaskState::Failed);
        assert!(
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .error
                .unwrap()
                .contains("timed out")
        );

        // A request that stops answering is dead: the actor must reconnect, which
        // announces the outage (machine.lost) and its recovery (machine.connected),
        // just like `disconnect_triggers_reconnect_and_resubscribe`.
        let mut saw_lost = false;
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("machine.connected within 5s")
                .unwrap();
            if ev.kind == "machine.lost" {
                saw_lost = true;
            } else if ev.kind == "machine.connected" {
                assert!(
                    saw_lost,
                    "machine.connected without a preceding machine.lost"
                );
                break;
            }
        }
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
    }

    /// A herdr that accepts the events connection and never acknowledges the
    /// subscription must not hang the actor: `open_events` bounds the subscribe
    /// by `request_timeout` and the attempt is retried after backoff.
    /// `hang_method` is one-shot, so only the first attempt is wedged.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_subscribe_reconnects() {
        let fake = FakeHerdr::new();
        fake.hang_method("events.subscribe");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut settings = settings();
        settings.request_timeout = Duration::from_millis(100);
        settings.agent_ready_timeout = Duration::from_millis(50);
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected despite the wedged subscribe", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
    }

    /// `reconcile` runs right after `ping` on every connect attempt, and its
    /// `agent.list` call had no timeout before this fix: a herdr that accepted the
    /// connection and then stopped answering there would hang the actor forever,
    /// which in turn blocks `Daemon::dispatch_queued` and the accept loop behind it.
    /// `hang_method` is one-shot, so this hangs only the first connect attempt's
    /// `agent.list`; the retry after backoff finds it answering normally again.
    #[tokio::test(start_paused = true)]
    async fn reconcile_over_a_wedged_connection_reconnects() {
        let fake = FakeHerdr::new();
        fake.hang_method("agent.list");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut settings = settings();
        settings.request_timeout = Duration::from_millis(100);
        settings.agent_ready_timeout = Duration::from_millis(50);
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected despite the wedged reconcile", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
    }

    /// `pastor task sweep` on one machine: a worktree of an ended task is
    /// removed only when every check passes, a dry run removes nothing, and
    /// whatever cannot be told is kept.
    mod sweep {
        use super::*;

        /// A task row on machine `m` in `state` recording `path` as its
        /// checkout.
        fn task_at(store: &Store, state: TaskState, path: &str) -> Task {
            let mut t = new_task(store);
            t.machine = Some("m".into());
            t.state = state;
            t.spec.worktree = true;
            t.spec.repo = Some("/srv/repo".into());
            t.spec.checkout = Some(Box::new(crate::task::Checkout {
                branch: format!("pastor/t-{}", t.id),
                path: path.into(),
                already_open: false,
            }));
            store.update_task(&mut t).unwrap();
            t
        }

        const PATH: &str = "/srv/worktrees/repo/t-1";

        fn kept_why(entries: &[SweepEntry]) -> Vec<(SweepAction, Option<String>)> {
            entries.iter().map(|e| (e.action, e.why.clone())).collect()
        }

        /// A clean worktree of a failed or closed task is removed,
        /// once, whatever the rows that record it say of each other.
        #[tokio::test(start_paused = true)]
        async fn a_clean_kept_worktree_is_removed() {
            for state in [TaskState::Failed, TaskState::Closed] {
                let fake = FakeHerdr::new();
                let store = Arc::new(Store::open_in_memory().unwrap());
                let (h, _events) = connected(&fake, &store).await;
                let t = task_at(&store, state, PATH);
                // A retry that has ended too records the same worktree.
                let retry = task_at(&store, TaskState::Closed, PATH);
                let got = h.sweep(true).await.unwrap();
                assert_eq!(got.len(), 1, "{state:?}: {got:?}");
                assert_eq!(got[0].action, SweepAction::Removed, "{got:?}");
                assert_eq!(got[0].path, PATH);
                assert_eq!(got[0].branch, format!("pastor/t-{}", t.id));
                assert_eq!(got[0].tasks, [t.display_id(), retry.display_id()]);
                assert_eq!(fake.removed_checkouts(), [PATH], "{state:?}");
            }
        }

        /// A worktree the sweep removes takes the workdir sets placed in it
        /// out with it; a dry run takes nothing out.
        #[tokio::test(start_paused = true)]
        async fn a_swept_worktree_takes_its_workdir_sets_out() {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (settings, gate) = forgetting(MachineSettings::default());
            let (h, _events) = spawn_with_settings(&fake, &store, settings);
            wait_for("connected", || {
                h.snapshot().channel == ChannelState::Connected
            })
            .await;
            task_at(&store, TaskState::Failed, PATH);
            h.sweep(false).await.unwrap();
            assert!(gate.dirs().is_empty(), "a dry run");
            let got = h.sweep(true).await.unwrap();
            assert_eq!(got[0].action, SweepAction::Removed, "{got:?}");
            assert_eq!(gate.dirs(), [PATH]);
        }

        /// Without `apply` the same worktree is only listed.
        #[tokio::test(start_paused = true)]
        async fn a_dry_run_removes_nothing() {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            task_at(&store, TaskState::Failed, PATH);
            let got = h.sweep(false).await.unwrap();
            assert_eq!(kept_why(&got), [(SweepAction::WouldRemove, None)]);
            assert!(fake.removed_checkouts().is_empty());
        }

        #[tokio::test(start_paused = true)]
        async fn a_worktree_with_uncommitted_changes_is_kept() {
            let fake = FakeHerdr::new();
            fake.set_dirty_checkout(PATH);
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            task_at(&store, TaskState::Failed, PATH);
            for apply in [false, true] {
                let got = h.sweep(apply).await.unwrap();
                assert_eq!(
                    kept_why(&got),
                    [(
                        SweepAction::Kept,
                        Some("uncommitted or untracked files".into())
                    )]
                );
            }
            assert!(
                fake.removed_checkouts().is_empty(),
                "never asked to remove it"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn a_worktree_with_unpushed_commits_is_kept() {
            let fake = FakeHerdr::new();
            fake.set_unpushed(PATH);
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            task_at(&store, TaskState::Closed, PATH);
            let got = h.sweep(true).await.unwrap();
            assert_eq!(
                kept_why(&got),
                [(SweepAction::Kept, Some("commits on no remote".into()))]
            );
            assert!(fake.removed_checkouts().is_empty());
        }

        /// A live task that records the worktree (a retry), or works in it as
        /// its repo (a fix round), keeps it; a running task's own worktree is
        /// not listed at all.
        #[tokio::test(start_paused = true)]
        async fn the_worktree_of_a_live_task_is_kept() {
            for state in [
                TaskState::Running,
                TaskState::Queued,
                TaskState::Paused,
                TaskState::Waiting,
                TaskState::Stale,
                TaskState::Done,
            ] {
                let fake = FakeHerdr::new();
                let store = Arc::new(Store::open_in_memory().unwrap());
                let (h, _events) = connected(&fake, &store).await;
                task_at(&store, TaskState::Failed, PATH);
                let live = task_at(&store, state, PATH);
                let got = h.sweep(true).await.unwrap();
                assert_eq!(got.len(), 1, "{state:?}: {got:?}");
                assert_eq!(got[0].action, SweepAction::Kept, "{state:?}");
                assert!(
                    got[0]
                        .why
                        .as_deref()
                        .is_some_and(|w| w.contains(&live.display_id())),
                    "{got:?}"
                );
                assert!(fake.removed_checkouts().is_empty(), "{state:?}");
            }
            // A fix round whose --repo is the worktree.
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            task_at(&store, TaskState::Failed, PATH);
            let mut round = new_task(&store);
            round.machine = Some("m".into());
            round.state = TaskState::Running;
            round.spec.repo = Some(PATH.into());
            store.update_task(&mut round).unwrap();
            let got = h.sweep(true).await.unwrap();
            assert_eq!(got[0].action, SweepAction::Kept, "{got:?}");
            assert!(fake.removed_checkouts().is_empty());
            // Alone, a running task's worktree is not the sweep's business.
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            task_at(&store, TaskState::Running, PATH);
            assert!(h.sweep(true).await.unwrap().is_empty());
        }

        /// What pastor did not make, what is not a path and what is not
        /// there is never removed.
        #[tokio::test(start_paused = true)]
        async fn what_pastor_did_not_make_or_cannot_name_is_kept_or_skipped() {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let mut shown = task_at(&store, TaskState::Closed, "/srv/worktrees/repo/shown");
            shown.spec.checkout.as_mut().unwrap().already_open = true;
            store.update_task(&mut shown).unwrap();
            for bad in [
                "/",
                "/home",
                "/srv/x",
                "relative/dir/here",
                "/srv/../etc/passwd",
            ] {
                task_at(&store, TaskState::Closed, bad);
            }
            let gone = "/srv/worktrees/repo/gone";
            fake.set_missing_dir(gone);
            task_at(&store, TaskState::Closed, gone);
            let got = h.sweep(true).await.unwrap();
            assert_eq!(got.len(), 6, "{got:?}");
            assert!(got.iter().all(|e| e.action == SweepAction::Kept), "{got:?}");
            assert!(!got.iter().any(|e| e.path == gone), "gone is not listed");
            let shown = got.iter().find(|e| e.path.ends_with("/shown")).unwrap();
            assert!(
                shown
                    .why
                    .as_deref()
                    .is_some_and(|w| w.contains("did not open")),
                "{shown:?}"
            );
            assert!(
                fake.removed_checkouts().is_empty(),
                "{:?}",
                fake.removed_checkouts()
            );
        }

        /// Another machine's rows are not this machine's to sweep.
        #[tokio::test(start_paused = true)]
        async fn another_machines_worktrees_are_left_alone() {
            let fake = FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let (h, _events) = connected(&fake, &store).await;
            let mut t = task_at(&store, TaskState::Closed, PATH);
            t.machine = Some("elsewhere".into());
            store.update_task(&mut t).unwrap();
            assert!(h.sweep(true).await.unwrap().is_empty());
            assert!(fake.removed_checkouts().is_empty());
        }
    }

    /// A task whose agent has finished its prompt and sits idle: the one a
    /// move asks to save its work. The fake, the store, the handle, the row.
    async fn idle_task() -> (FakeHerdr, Arc<Store>, MachineHandle, Task) {
        let (fake, store, h, t, _events) = idle_task_and_events().await;
        (fake, store, h, t)
    }

    /// `idle_task`, and the machine's events from then on.
    async fn idle_task_and_events() -> (
        FakeHerdr,
        Arc<Store>,
        MachineHandle,
        Task,
        broadcast::Receiver<PastorEvent>,
    ) {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        let t = store.get_task(t.id).unwrap().unwrap();
        while events.try_recv().is_ok() {}
        (fake, store, h, t, events)
    }

    fn refused_code(err: &anyhow::Error) -> Option<&'static str> {
        err.downcast_ref::<SendRefused>().map(|r| r.code)
    }

    /// The `task.*` events for task `id` so far.
    fn task_events(events: &mut broadcast::Receiver<PastorEvent>, id: i64) -> Vec<String> {
        let mut out = vec![];
        while let Ok(ev) = events.try_recv() {
            if ev.task_id == Some(id) && ev.kind.starts_with("task.") {
                out.push(ev.kind);
            }
        }
        out
    }

    /// The save turn is not the task's work: while it runs, and after it,
    /// whether it saved or not, the row keeps its state and nothing is
    /// emitted for it. A done Claude task whose save times out mid-turn,
    /// and a non-Claude task blocked idle without `task done` whose save
    /// goes through, each stay as they were; a later idle at the save's
    /// sequence completes nothing.
    #[tokio::test(start_paused = true)]
    async fn the_save_turn_leaves_the_row_alone() {
        // A done Claude task; the save times out while the agent works.
        let (fake, store, h, t, mut events) = idle_task_and_events().await;
        let pane = t.pane_id.clone().unwrap();
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(5)).await }
        });
        wait_for("the save prompt", || save_typed(&fake)).await;
        fake.set_status(&pane, AgentStatus::Working);
        let err = saving.await.unwrap().unwrap_err();
        assert_eq!(refused_code(&err), Some("save_timeout"), "{err:#}");
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Done);
        assert_eq!(task_events(&mut events, t.id), Vec::<String>::new());

        // A codex task blocked for going idle without `task done`.
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(new_task_of(&store, "codex").id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        task_events(&mut events, t.id);
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(60)).await }
        });
        wait_for("the save prompt", || save_typed(&fake)).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        fake.set_status(&pane, AgentStatus::Idle);
        let saved = saving.await.unwrap().unwrap();
        assert_eq!(saved.state, TaskState::Blocked);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert_eq!(task_events(&mut events, t.id), Vec::<String>::new());
        let row = store.get_task(t.id).unwrap().unwrap();
        assert!(row.last_completion_seq >= Some(fake.agents()[0].state_change_seq));
    }

    /// An agent that stops on a question during the save turn fails it at
    /// once, not at the end of the timeout.
    #[tokio::test(start_paused = true)]
    async fn a_question_during_the_save_turn_is_save_blocked_at_once() {
        let (fake, _store, h, t) = idle_task().await;
        let pane = t.pane_id.clone().unwrap();
        let started = Instant::now();
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(600)).await }
        });
        wait_for("the save prompt", || save_typed(&fake)).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        fake.set_status(&pane, AgentStatus::Blocked);
        let err = saving.await.unwrap().unwrap_err();
        assert_eq!(refused_code(&err), Some("save_blocked"), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    /// A save turn shorter than a poll, on a herdr with no
    /// `completion_seq`: the poll only ever sees the agent idle, but the
    /// actor saw it working, so the save is done.
    #[tokio::test(start_paused = true)]
    async fn a_save_turn_shorter_than_a_poll_is_seen() {
        let (fake, _store, h, t) = idle_task().await;
        fake.set_inbox_result(Some("accepted"));
        let pane = t.pane_id.clone().unwrap();
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(30)).await }
        });
        wait_for("the save prompt", || save_by_socket(&fake) == 1).await;
        // The send set it working; done again before the first poll.
        tokio::time::sleep(Duration::from_millis(50)).await;
        fake.set_status(&pane, AgentStatus::Idle);
        let saved = tokio::time::timeout(Duration::from_secs(10), saving)
            .await
            .expect("the save ends well before its timeout")
            .unwrap()
            .unwrap();
        assert_eq!(saved.id, t.id);
    }

    /// The save prompt goes in, typed on a machine with no inbox socket
    /// send, and the reply waits until the agent has worked on it and gone
    /// idle again.
    #[tokio::test(start_paused = true)]
    async fn save_work_prompts_and_waits_for_idle() {
        let (fake, _store, h, t) = idle_task().await;
        let pane = t.pane_id.clone().unwrap();
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(60)).await }
        });
        wait_for("the save prompt", || {
            calls(&fake, "agent.prompt")
                .iter()
                .any(|p| p["text"] == crate::task::MOVE_SAVE_PROMPT)
        })
        .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!saving.is_finished(), "the agent is still working");
        fake.set_status(&pane, AgentStatus::Idle);
        let saved = saving.await.unwrap().unwrap();
        assert_eq!(saved.id, t.id);
    }

    fn save_typed(fake: &FakeHerdr) -> bool {
        calls(fake, "agent.prompt")
            .iter()
            .any(|p| p["text"] == crate::task::MOVE_SAVE_PROMPT)
    }

    fn save_by_socket(fake: &FakeHerdr) -> usize {
        fake.inbox_sends()
            .iter()
            .filter(|s| s.text == crate::task::MOVE_SAVE_PROMPT)
            .count()
    }

    /// A Claude session that takes the save prompt through its inbox socket
    /// gets it there, not typed; the save counts from just before the send.
    #[tokio::test(start_paused = true)]
    async fn save_work_goes_through_the_inbox_socket() {
        let (fake, _store, h, t) = idle_task().await;
        fake.set_inbox_result(Some("accepted"));
        let pane = t.pane_id.clone().unwrap();
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(60)).await }
        });
        wait_for("the save prompt", || save_by_socket(&fake) == 1).await;
        let sent = &fake.inbox_sends()[0];
        assert_eq!(Some(sent.session.as_str()), t.spec.session_id.as_deref());
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!saving.is_finished(), "the agent is still working");
        assert!(!save_typed(&fake), "not typed");
        fake.set_status(&pane, AgentStatus::Idle);
        assert_eq!(saving.await.unwrap().unwrap().id, t.id);
        assert_eq!(save_by_socket(&fake), 1);
    }

    /// A socket that does not take it, and a task with no session to send
    /// it to, get the save prompt typed instead.
    #[tokio::test(start_paused = true)]
    async fn save_work_the_socket_does_not_take_is_typed() {
        for no_session in [false, true] {
            let (fake, store, h, mut t) = idle_task().await;
            if no_session {
                fake.set_inbox_result(Some("accepted"));
                t.spec.session_id = None;
                store.update_task(&mut t).unwrap();
            } else {
                fake.set_inbox_result(Some("not_running"));
            }
            let pane = t.pane_id.clone().unwrap();
            let saving = tokio::spawn({
                let h = h.clone();
                async move { h.save_work(t.id, Duration::from_secs(60)).await }
            });
            wait_for("the typed save prompt", || save_typed(&fake)).await;
            assert_eq!(save_by_socket(&fake), usize::from(!no_session));
            fake.set_status(&pane, AgentStatus::Idle);
            assert_eq!(saving.await.unwrap().unwrap().id, t.id);
        }
    }

    /// An agent waiting on a question would get the save prompt in its
    /// dialog: refused, nothing sent either way.
    #[tokio::test(start_paused = true)]
    async fn save_work_refuses_a_blocked_agent() {
        let (fake, _store, h, t) = idle_task().await;
        fake.set_inbox_result(Some("accepted"));
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        let err = h
            .save_work(t.id, Duration::from_secs(60))
            .await
            .unwrap_err();
        assert_eq!(refused_code(&err), Some("save_blocked"), "{err:#}");
        assert!(!save_typed(&fake));
        assert_eq!(save_by_socket(&fake), 0);
    }

    /// A working agent is in the middle of something: a save prompt would
    /// land in its turn, so it is refused and nothing is typed.
    #[tokio::test(start_paused = true)]
    async fn save_work_refuses_a_working_agent() {
        let (fake, _store, h, t) = idle_task().await;
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Working);
        let prompts = calls(&fake, "agent.prompt").len();
        let err = h
            .save_work(t.id, Duration::from_secs(60))
            .await
            .unwrap_err();
        assert_eq!(refused_code(&err), Some("agent_busy"), "{err:#}");
        assert_eq!(calls(&fake, "agent.prompt").len(), prompts);
    }

    /// A task with no pane has no agent to save anything.
    #[tokio::test(start_paused = true)]
    async fn save_work_refuses_a_task_without_a_pane() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let queued = new_task(&store);
        let err = h
            .save_work(queued.id, Duration::from_secs(60))
            .await
            .unwrap_err();
        assert_eq!(refused_code(&err), Some("task_not_live"), "{err:#}");
        assert!(calls(&fake, "agent.prompt").is_empty());
    }

    /// An agent that never finishes the save is `save_timeout`; the pane
    /// and the row stay as they were, for the caller to decide.
    #[tokio::test(start_paused = true)]
    async fn save_work_times_out_and_leaves_the_pane() {
        let (fake, store, h, t) = idle_task().await;
        fake.ignore_prompts(true);
        let err = h.save_work(t.id, Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(refused_code(&err), Some("save_timeout"), "{err:#}");
        assert!(closes(&fake).is_empty(), "the pane stays");
        let pane = t.pane_id.as_deref().unwrap();
        assert!(fake.agents().iter().any(|a| a.pane_id == pane));
        let row = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Done);
        assert_eq!(row.pane_id, t.pane_id);
    }

    /// A done task whose agent accepts the save prompt but never reacts to
    /// it (`fake.ignore_prompts`, as `save_work_times_out_and_leaves_the_pane`
    /// uses it) reads exactly like one that finished on its own and has sat
    /// idle since: same pane, same agent status, same sequence. Without
    /// tracking the save in flight, `auto_close_done` cannot tell the two
    /// apart, and would take its pane once the grace elapses while the save
    /// is still waiting on the agent.
    #[tokio::test(start_paused = true)]
    async fn save_work_in_progress_is_not_auto_closed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(&fake, &store, auto_close_settings());
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;

        fake.ignore_prompts(true);
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(60)).await }
        });
        wait_for("the save prompt", || {
            calls(&fake, "agent.prompt")
                .iter()
                .any(|p| p["text"] == crate::task::MOVE_SAVE_PROMPT)
        })
        .await;

        // Well past its grace: without the guard this looks exactly like a
        // task that finished on its own and has sat idle since, and
        // auto-close would take its pane mid-save.
        finished_an_hour_ago(&store, t.id);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Done,
            "a task being saved is not auto-closed"
        );
        assert!(
            calls(&fake, "pane.close").is_empty(),
            "the pane stays while the save is in flight"
        );

        // The agent never reacted, so the save times out; once it is no
        // longer in flight, the next check closes the row.
        let err = saving.await.unwrap().unwrap_err();
        assert_eq!(refused_code(&err), Some("save_timeout"), "{err:#}");
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
    }

    /// The preflight check for the agent is bounded by the save's own
    /// `timeout`, not the full `request_timeout`: a herdr that stops
    /// answering there is `save_timeout` at the deadline the save was asked
    /// to keep, the same as one that stops answering during the wait.
    #[tokio::test(start_paused = true)]
    async fn save_work_preflight_is_bounded_by_the_save_timeout() {
        let (fake, _store, h, t) = idle_task().await;
        fake.hang_method("agent.list");
        let started = Instant::now();
        let err = h
            .save_work(t.id, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(refused_code(&err), Some("save_timeout"), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "bounded by the save's own timeout, not the 5s request_timeout: {:?}",
            started.elapsed()
        );
    }

    /// Idle past the save prompt's own sequence is not proof the save
    /// finished: the agent may have stopped to ask a question instead (over
    /// a force-push, say). Reading the pane before trusting that, as
    /// `pane_end` does for a real completion, catches it instead of
    /// reporting a save that never happened.
    #[tokio::test(start_paused = true)]
    async fn save_work_rejects_a_question_instead_of_a_finished_save() {
        let (fake, _store, h, t) = idle_task().await;
        let pane = t.pane_id.clone().unwrap();
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(60)).await }
        });
        wait_for("the save prompt", || {
            calls(&fake, "agent.prompt")
                .iter()
                .any(|p| p["text"] == crate::task::MOVE_SAVE_PROMPT)
        })
        .await;
        fake.set_pane_text(
            &pane,
            "● Should I force-push over the remote branch?\n\n❯\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        let err = saving.await.unwrap().unwrap_err();
        assert_eq!(refused_code(&err), Some("save_blocked"), "{err:#}");
    }

    /// A save prompt answered by a command left running in the background is
    /// not finished either: `save_work` keeps polling past the first idle
    /// instead of calling it done on the shell's own pause, the same rule
    /// `pane_end` applies to a real completion.
    #[tokio::test(start_paused = true)]
    async fn save_work_waits_out_a_background_shell() {
        let (fake, _store, h, t) = idle_task().await;
        let pane = t.pane_id.clone().unwrap();
        let saving = tokio::spawn({
            let h = h.clone();
            async move { h.save_work(t.id, Duration::from_secs(60)).await }
        });
        wait_for("the save prompt", || {
            calls(&fake, "agent.prompt")
                .iter()
                .any(|p| p["text"] == crate::task::MOVE_SAVE_PROMPT)
        })
        .await;
        fake.set_pane_text(
            &pane,
            "● Committing in the background.\n\n❯\n  ⏵⏵ auto mode on · 1 shell still running\n",
        );
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !saving.is_finished(),
            "a background shell is not a finished save"
        );

        fake.set_status(&pane, AgentStatus::Working);
        fake.set_pane_text(&pane, "● Committed and pushed.\n\n❯\n");
        fake.set_status(&pane, AgentStatus::Idle);
        let saved = saving.await.unwrap().unwrap();
        assert_eq!(saved.id, t.id);
    }
}
