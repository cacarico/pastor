use std::path::Path;

use std::collections::BTreeMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};

fn default_session() -> String {
    "default".to_string()
}
fn default_max_agents() -> u32 {
    2
}
fn default_one() -> u32 {
    1
}
fn is_one(n: &u32) -> bool {
    *n == 1
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
    /// A machine the head never connects to: its own headless `pastor
    /// serve` asks the head for tasks (`IpcRequest::TaskClaim`) and runs
    /// them itself, reporting each change (`IpcRequest::TaskReport`). The
    /// head runs no actor for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pull: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    /// Developer option: argv speaking the herdr protocol on stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default = "default_session")]
    pub session: String,
    #[serde(default = "default_max_agents")]
    pub max_agents: u32,
    /// Slots on top of `max_agents` that only tasks from jobs take; `0`
    /// turns them off (see `dispatch::MachineView::has_room`).
    #[serde(default = "default_one", skip_serializing_if = "is_one")]
    pub job_slots: u32,
    /// How many tasks past `max_agents` a `critical` task may start; `0`
    /// turns it off.
    #[serde(default = "default_one", skip_serializing_if = "is_one")]
    pub burst: u32,
    /// Agent slots of `max_agents` kept free for panes pastor did not
    /// start: dispatch places at most `max_agents - reserved` of its own
    /// tasks there. `task run --now` looks past it. At most `max_agents`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reserved: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The loads this machine takes (`rust`, `light`), on top of those of
    /// the flocks it is in (`Flock::loads_of`): a task with a load goes
    /// only to a machine that takes it (`DispatchSpec::load`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loads: Vec<String>,
    /// The old way to put a machine in a flock: membership in it with the
    /// machine's own limits (`max_agents`, job slots, burst) as the flock's
    /// number here. Kept as written; `None` adds nothing (see
    /// `Flock::flocks_of`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flock: Option<String>,
    /// The agent for tasks on this machine that name none, before its
    /// flock's and `[defaults]` (see `Defaults::resolve_agent_on`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Like `agent`, for the agent's args; `[]` means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_args: Option<Vec<String>>,
    /// The `[models]` name for tasks on this machine that name none, after
    /// its flock's and before `[defaults]` (`Defaults::resolve_agent_on`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The `[models]` names tasks on this machine may fall back to when
    /// their run flags or job set none, before its flock's and `[defaults]`;
    /// `[]` means none (`Defaults::resolve_agent_on`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Vec<String>>,
    /// The level of tasks pinned to this machine that name none, after
    /// its flock's and before `[defaults]` (see `Defaults::resolve_priority`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<crate::task::Priority>,
    /// The agent that runs a model of another kind than `agent`'s here, by
    /// kind (see `Defaults::resolve_agent_for`).
    #[serde(default, skip_serializing_if = "crate::config::KindAgents::is_empty")]
    pub agents: crate::config::KindAgents,
    /// The permission profile for tasks on this machine that name none,
    /// after its flock's and before `[defaults]`. Also what decides whether a task
    /// may ask for `unrestricted` here (`Profiles::apply`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The disk this machine must have free, on each of `$HOME` and `/tmp`,
    /// to take a task, as a size (`"4G"`); `"0"` is no disk floor. Setting
    /// it, or `min_memory`, turns the machine's room floor on; the other
    /// side then comes from `[defaults]`, else the built-in size
    /// (`Defaults::resolve_floor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_disk: Option<String>,
    /// Like `min_disk`, for free memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_memory: Option<String>,
    /// How this machine counts against a floor while its room cannot be
    /// read, after its flock's (`config::resolve_unknown_room`); unset,
    /// as room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_room: Option<crate::config::UnknownRoom>,
    /// The provisioner that checks and sets this machine up for agents: a
    /// built-in id (`macos`, `raspberry-pi-os`, `arch`) or one copied into
    /// the data dir (`provision::Catalog`). Unset: `machine check` has
    /// nothing to run here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioner: Option<String>,
    /// Bundles this machine gets on top of its flocks' (`bundle::Catalog`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundles: Vec<String>,
    /// How this machine takes a new version of its bundles, over its
    /// flocks' (`Flock::bundle_apply`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_apply: Option<crate::bundle::Apply>,
    /// Harness version pins for this machine, per kind (`claude`, `agy`),
    /// over its flock's `pins`: `pins = { claude = "2.1.310" }`. Checked,
    /// not enforced (see `harness`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pins: crate::harness::Pins,
    /// Environment for the panes of every agent on this machine, over its
    /// flock's and `[defaults]` and under the agent's own
    /// (`config::EnvLayers`). A value that looks like a secret is refused.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// One line on what the machine is for (`machine list --wide`,
    /// `describe`); nothing reads it but people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A flock's number on a machine: a plain ceiling (`desk = 2`), or a share
/// and a max (`desk = { share = 2, max = 4 }`). Under its share the flock
/// takes a free slot as usual; from its share up to its max it takes one
/// only while no task of a flock under its share there is waiting. The
/// plain form is a share equal to the max. 0 keeps the machine in the
/// flock with no slots: its tasks there run to the end and no new one
/// starts, to drain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FlockNumber {
    Plain(u32),
    Split(SplitNumber),
}

/// The table form of `FlockNumber`, as written. Both keys must be there;
/// `Flock::validate` says so, with the flock and machine in the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitNumber {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
}

impl FlockNumber {
    pub fn plain(n: u32) -> FlockNumber {
        FlockNumber::Plain(n)
    }

    pub fn split(share: u32, max: u32) -> FlockNumber {
        FlockNumber::Split(SplitNumber {
            share: Some(share),
            max: Some(max),
        })
    }

    /// Up to how many live tasks the flock takes a free slot as usual.
    pub fn share(&self) -> u32 {
        match self {
            FlockNumber::Plain(n) => *n,
            FlockNumber::Split(s) => s.share.or(s.max).unwrap_or(0),
        }
    }

    /// The hard ceiling: never more of the flock's live tasks than this.
    pub fn max(&self) -> u32 {
        match self {
            FlockNumber::Plain(n) => *n,
            FlockNumber::Split(s) => s.max.or(s.share).unwrap_or(0),
        }
    }

    /// Why the number does not load, if it does not.
    fn problem(&self) -> Option<String> {
        match *self {
            FlockNumber::Plain(_) => None,
            FlockNumber::Split(SplitNumber { share: None, .. }) => {
                Some("max alone is not allowed; a plain number (`= N`) is the hard ceiling".into())
            }
            FlockNumber::Split(SplitNumber { max: None, .. }) => {
                Some("a share needs a max; a plain number (`= N`) is the hard ceiling".into())
            }
            FlockNumber::Split(SplitNumber {
                share: Some(share),
                max: Some(max),
            }) => {
                if share == 0 && max > 0 {
                    Some("the share must be at least 1 while max is above 0".into())
                } else if max < share {
                    Some(format!("max {max} is below share {share}"))
                } else {
                    None
                }
            }
        }
    }
}

/// `2`, or `2/4` for a share of 2 and a max of 4.
impl std::fmt::Display for FlockNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.share() == self.max() {
            write!(f, "{}", self.max())
        } else {
            write!(f, "{}/{}", self.share(), self.max())
        }
    }
}

/// The flock a file with no `[[flock]]` entry has: every machine is in it.
pub const DEFAULT_FLOCK: &str = "default";

/// One `[[flock]]` entry: a name, the machines it may use, and the agent its
/// tasks get when the task or job says nothing. A machine can be in many
/// flocks (see `Flock::flocks_of`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlockEntry {
    pub name: String,
    /// Where tasks and jobs that name no flock go. Exactly one entry has it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// The machines this flock may use, each with at most how many of the
    /// flock's live tasks it runs: `machines = { desk = 2 }`, or a share and
    /// a max (`FlockNumber`). Job slots and burst never take a machine past
    /// its max.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub machines: BTreeMap<String, FlockNumber>,
    /// The agent for this flock's tasks that name none; `None` falls through
    /// to `[defaults] agent` (see `Defaults::resolve_agent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Like `agent`, for the agent's args; `[]` means none, not `[defaults]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_args: Option<Vec<String>>,
    /// Tool patterns this flock's agents may use, on top of `[defaults]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Tool patterns this flock's agents must not use, on top of `[defaults]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// The `[models]` name for this flock's tasks that name none, before
    /// `[defaults] model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The `[models]` names this flock's tasks may fall back to when their
    /// run flags, job and machine set none, before `[defaults] fallback`;
    /// `[]` means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Vec<String>>,
    /// The level of this flock's tasks that name none, before `[defaults]
    /// priority`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<crate::task::Priority>,
    /// Like the machine's `agents`, before `[defaults] agents`.
    #[serde(default, skip_serializing_if = "crate::config::KindAgents::is_empty")]
    pub agents: crate::config::KindAgents,
    /// The permission profile for this flock's tasks that name none, before
    /// `[defaults] profile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The version of each harness's CLI this flock expects on its
    /// machines, per kind: `pins = { claude = "2.1.310", agy = "1.4" }`. A
    /// part left out is any (`"2.1"` takes `2.1.310`), `"latest"` pins
    /// nothing. A `[[machine]]` entry's `pins` replace a kind's here. Checked
    /// by `machine check` and shown by `machine list`; nothing installs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pins: crate::harness::Pins,
    /// Environment for the panes of every agent of this flock's tasks, over
    /// `[defaults] env` and under the machine's and the agent's own
    /// (`config::EnvLayers`). A value that looks like a secret is refused.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// How long this flock's tasks may run (`"2h"`) when the task or job
    /// sets none, before `[defaults] timeout` (`Defaults::resolve_timeout`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// How long this flock's queued tasks wait before they go up a level,
    /// or `never`, before `[defaults] age_after`
    /// (`Defaults::resolve_age_after`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_after: Option<String>,
    /// Where this flock's tasks put their pane when the task or job sets
    /// none, before `[defaults] place` (`Defaults::resolve_place`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<crate::task::Place>,
    /// The label template of the workspace this flock's tasks make when
    /// the task or job sets none, before `[defaults] label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether this flock's tasks are asked for a summary, or need one
    /// (`SummaryMode`), when the task and its job say nothing; before
    /// `[defaults] summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::task::SummaryMode>,
    /// Whether this flock's tasks keep their pane once they end, when the
    /// task and its job say nothing; before `[defaults] keep_pane`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_pane: Option<bool>,
    /// Whether this flock's tasks lose their worktree once they end, failed
    /// or stale too, when the task and its job say nothing; before
    /// `[defaults] remove_worktree`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_worktree: Option<bool>,
    /// How a machine whose room cannot be read counts for this flock's
    /// tasks, before the machine's own `unknown_room`
    /// (`config::resolve_unknown_room`); unset, as room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_room: Option<crate::config::UnknownRoom>,
    /// The accounts this flock's machines must be logged into, `github`
    /// and `claude` being the ones pastor knows how to check: a machine's
    /// provisioner runs the checks marked with them (`provision::Account`).
    /// A list of kinds, or a map from kind to the account name `machine
    /// check` expects the machine to be signed in as (`Accounts`).
    #[serde(default, skip_serializing_if = "Accounts::is_empty")]
    pub accounts: Accounts,
    /// The bundles this flock's machines get, by id (`bundle::Catalog`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundles: Vec<String>,
    /// The workdir bundles (`target = "workdir"`) placed in the working
    /// directory of each of this flock's tasks before its agent starts, by
    /// id (`bundle::workdir_stored`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workdir_bundles: Vec<String>,
    /// Values for the vars its bundles declare, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bundle_vars: BTreeMap<String, String>,
    /// How its machines take a new version of their bundles; unset is
    /// `manual`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_apply: Option<crate::bundle::Apply>,
    /// The loads every machine of this flock takes (`rust`, `light`), for
    /// any flock's task placed there, not only this one's
    /// (`Flock::loads_of`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loads: Vec<String>,
    /// One line on what the flock is for (`flock list --wide`, `describe`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Why a task cannot have the flock it asked for (`Flock::task_flock`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskFlockError {
    #[error("flock {0} does not exist")]
    UnknownFlock(String),
    #[error("machine {machine} is in {}, not {requested}", flocks_phrase(flocks))]
    MachineElsewhere {
        machine: String,
        flocks: Vec<String>,
        requested: String,
    },
}

/// Who names a bundle in flock.toml (`Flock::bundle_users`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BundleUsers {
    /// Flocks whose `bundles` list it.
    pub flocks: Vec<String>,
    /// Machines whose own `bundles` list it.
    pub machines: Vec<String>,
    /// Every machine that gets it, through a flock or its own list.
    pub affected: Vec<String>,
}

impl BundleUsers {
    pub fn is_empty(&self) -> bool {
        self.flocks.is_empty() && self.machines.is_empty()
    }
}

/// A `bundles` list: bundle ids, each once.
fn check_bundle_ids(ids: &[String]) -> Result<(), String> {
    for (i, id) in ids.iter().enumerate() {
        crate::connector::manifest::check_id(id)?;
        if ids[..i].contains(id) {
            return Err(format!("{id} is listed twice"));
        }
    }
    Ok(())
}

/// A `loads` list: load names, each once.
fn check_loads(loads: &[String]) -> Result<(), String> {
    for (i, load) in loads.iter().enumerate() {
        crate::task::check_load(load)?;
        if loads[..i].contains(load) {
            return Err(format!("{load} is listed twice"));
        }
    }
    Ok(())
}

/// `flock a` or `flocks a, b`.
fn flocks_phrase(flocks: &[String]) -> String {
    match flocks {
        [one] => format!("flock {one}"),
        many => format!("flocks {}", many.join(", ")),
    }
}

/// `flock.toml`: the declared flocks and the machines, each in one or more
/// of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flock {
    /// Empty means one implicit flock, `DEFAULT_FLOCK`, holding every machine.
    #[serde(default, rename = "flock", skip_serializing_if = "Vec::is_empty")]
    pub flocks: Vec<FlockEntry>,
    #[serde(default, rename = "machine")]
    pub machines: Vec<MachineConfig>,
}

/// What a machine name may be: the one rule for the name in a flock, in a
/// forced `authorized_keys` command and in a marker (`authkeys::valid_name`).
pub const MACHINE_NAME_RULE: &str =
    "start with a letter or digit, then letters, digits, '.', '_' and '-'";

/// A flock's `accounts`: the kinds its machines must be signed in to, as a
/// list (`["github", "claude"]`) or as a map from kind to the account name
/// `machine check` expects (`{ github = "alice", claude = "alice@example.com" }`).
/// The list form names no account, so it checks only that a login exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Accounts {
    list: Vec<crate::provision::Account>,
    names: BTreeMap<crate::provision::Account, String>,
}

impl Accounts {
    /// The list form: these kinds, no names.
    pub fn of(list: Vec<crate::provision::Account>) -> Accounts {
        Accounts {
            list,
            names: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &crate::provision::Account> {
        self.list.iter()
    }

    pub fn contains(&self, account: &crate::provision::Account) -> bool {
        self.list.contains(account)
    }

    /// The account name `machine check` expects for `account`; None in the
    /// list form, and for a kind the map does not name.
    pub fn name(&self, account: crate::provision::Account) -> Option<&str> {
        self.names.get(&account).map(String::as_str)
    }

    /// A kind listed twice, if one is.
    fn duplicate(&self) -> Option<crate::provision::Account> {
        self.list
            .iter()
            .enumerate()
            .find(|(i, a)| self.list[..*i].contains(a))
            .map(|(_, a)| *a)
    }
}

impl Serialize for Accounts {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.names.is_empty() {
            self.list.serialize(s)
        } else {
            self.names.serialize(s)
        }
    }
}

impl<'de> Deserialize<'de> for Accounts {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Accounts, D::Error> {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Accounts;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a list of accounts or a map from account to its name")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Accounts, A::Error> {
                let mut list = Vec::new();
                while let Some(a) = seq.next_element::<crate::provision::Account>()? {
                    list.push(a);
                }
                Ok(Accounts::of(list))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Accounts, A::Error> {
                let mut out = Accounts::default();
                while let Some((account, name)) =
                    map.next_entry::<crate::provision::Account, String>()?
                {
                    out.list.push(account);
                    out.names.insert(account, name);
                }
                Ok(out)
            }
        }
        d.deserialize_any(V)
    }
}

impl Flock {
    pub fn load(path: &Path) -> anyhow::Result<Flock> {
        if !path.exists() {
            return Ok(Flock::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Flock::parse(path, &text)
    }

    /// Like `load`, but a missing file is an error rather than the defaults:
    /// for a reload, where the `exists()` check and the read used to race a
    /// concurrent delete-and-rewrite and could momentarily see no machines.
    /// One read; the error keeps `std::io::ErrorKind::NotFound` at the top so
    /// callers can tell "missing" from "does not parse".
    pub fn load_existing(path: &Path) -> anyhow::Result<Flock> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::new(e)
            } else {
                anyhow::Error::new(e).context(format!("read {}", path.display()))
            }
        })?;
        Flock::parse(path, &text)
    }

    /// `text` as the file at `path` would load: parsed and validated, with
    /// errors that name `path`. `pastor flock edit` checks an edit with it.
    pub fn parse(path: &Path, text: &str) -> anyhow::Result<Flock> {
        // The toml error in the message, not a context under it: the reload
        // logs `%err`, which shows only the top, and the key must be there.
        let flock: Flock =
            toml::from_str(text).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        flock
            .validate()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        // A name from before the rule loads, and is said once for each
        // version of the file: a command reads flock.toml many times.
        flock.warn_odd_machine_names(path, text);
        Ok(flock)
    }

    /// Log `machine_name_warnings` of the flock `text` read from `path`, each
    /// at most once in this process for the file, what it says and the name:
    /// a command that reads an unchanged file many times says it once, and
    /// `pastor serve` says it again for each change of the file.
    fn warn_odd_machine_names(&self, path: &Path, text: &str) {
        use std::hash::{Hash, Hasher};
        static SAID: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::OnceLock::new();
        let said = SAID.get_or_init(Default::default);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let version = hasher.finish();
        for (m, w) in self
            .machines
            .iter()
            .filter(|m| !crate::authkeys::valid_name(&m.name))
            .zip(self.machine_name_warnings())
        {
            let first = said
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(format!("{}\0{version:x}\0{}", path.display(), m.name));
            if first {
                tracing::warn!(file = %path.display(), "{w}");
            }
        }
    }

    /// One line for each machine whose name `machine add` would refuse
    /// (`MACHINE_NAME_RULE`). A flock written before the rule still loads
    /// with them: the name only fails later, where it goes into an
    /// `authorized_keys` command, so it is said now and left to rename.
    pub fn machine_name_warnings(&self) -> Vec<String> {
        self.machines
            .iter()
            .filter(|m| !crate::authkeys::valid_name(&m.name))
            .map(|m| {
                format!(
                    "machine {:?} breaks the naming rule ({MACHINE_NAME_RULE}); rename it, as `machine authorized-key` and the bridge will refuse it",
                    m.name
                )
            })
            .collect()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.validate().map_err(|e| anyhow::anyhow!(e))?;
        if let Some(parent) = path.parent() {
            crate::config::create_private_dir(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut names = std::collections::HashSet::new();
        for f in &self.flocks {
            if f.name.is_empty() {
                return Err("flock with empty name".into());
            }
            if !names.insert(&f.name) {
                return Err(format!("flock {} listed twice", f.name));
            }
            crate::config::check_tools(&format!("flock {}: allow", f.name), &f.allow)?;
            crate::config::check_tools(&format!("flock {}: deny", f.name), &f.deny)?;
            if let Some(m) = &f.model {
                crate::config::check_model_name(m).map_err(|e| format!("flock {}: {e}", f.name))?;
            }
            for m in f.fallback.iter().flatten() {
                crate::config::check_model_name(m)
                    .map_err(|e| format!("flock {}: fallback: {e}", f.name))?;
            }
            crate::config::check_kind_agent_names(&f.agents)
                .map_err(|e| format!("flock {}: {e}", f.name))?;
            crate::harness::check_pins(&format!("flock {}: pins", f.name), &f.pins)?;
            crate::config::check_env(&format!("flock {}", f.name), &f.env)?;
            if let Some(p) = &f.profile {
                crate::config::check_profile_name(p)
                    .map_err(|e| format!("flock {}: {e}", f.name))?;
            }
            if let Some(label) = &f.label {
                crate::task::check_label(label).map_err(|e| format!("flock {}: {e}", f.name))?;
            }
            if let Some(t) = &f.timeout {
                crate::config::parse_duration(t)
                    .map_err(|e| format!("flock {}: timeout: {e}", f.name))?;
            }
            if let Some(v) = &f.age_after {
                crate::config::check_age_after(v)
                    .map_err(|e| format!("flock {}: age_after: {e}", f.name))?;
            }
            // The set itself is closed by the type: serde names the two
            // allowed values when the file has another.
            if let Some(twice) = f.accounts.duplicate() {
                return Err(format!(
                    "flock {}: accounts: {twice} is listed twice",
                    f.name
                ));
            }
            if let Some((account, _)) = f.accounts.names.iter().find(|(_, n)| n.trim().is_empty()) {
                return Err(format!(
                    "flock {}: accounts: the name for {account} is empty",
                    f.name
                ));
            }
            check_bundle_ids(&f.bundles).map_err(|e| format!("flock {}: bundles: {e}", f.name))?;
            check_bundle_ids(&f.workdir_bundles)
                .map_err(|e| format!("flock {}: workdir_bundles: {e}", f.name))?;
            check_loads(&f.loads).map_err(|e| format!("flock {}: loads: {e}", f.name))?;
            for name in f.bundle_vars.keys() {
                crate::bundle::manifest::check_var_name(name)
                    .map_err(|e| format!("flock {}: bundle_vars: {e}", f.name))?;
            }
        }
        if !self.flocks.is_empty() {
            let defaults: Vec<&str> = self
                .flocks
                .iter()
                .filter(|f| f.default)
                .map(|f| f.name.as_str())
                .collect();
            match defaults[..] {
                [_] => {}
                [] => return Err("no flock has default = true; exactly one must".into()),
                [a, b, ..] => {
                    return Err(format!(
                        "flocks {a} and {b} are both default; exactly one may be"
                    ));
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for m in &self.machines {
            if m.name.is_empty() {
                return Err("machine with empty name".into());
            }
            if !seen.insert(&m.name) {
                return Err(format!("machine {} listed twice", m.name));
            }
            let ways =
                m.local as u8 + m.ssh.is_some() as u8 + m.command.is_some() as u8 + m.pull as u8;
            if ways != 1 {
                return Err(format!(
                    "machine {}: set exactly one of local, ssh, command, pull",
                    m.name
                ));
            }
            if let Some(target) = &m.ssh
                && let Some(why) = ssh_target_problem(target)
            {
                return Err(format!("machine {}: ssh {target:?} {why}", m.name));
            }
            check_loads(&m.loads).map_err(|e| format!("machine {}: loads: {e}", m.name))?;
            if m.command.as_ref().is_some_and(|c| c.is_empty()) {
                return Err(format!("machine {}: command is empty", m.name));
            }
            // It goes into commands a remote login shell parses; fish reads
            // a backslash in single quotes as an escape (see `posix_command`).
            if m.session.contains('\\') || m.session.chars().any(char::is_control) {
                return Err(format!(
                    "machine {}: session {:?} contains a backslash or a control character",
                    m.name, m.session
                ));
            }
            if let Some(model) = &m.model {
                crate::config::check_model_name(model)
                    .map_err(|e| format!("machine {}: {e}", m.name))?;
            }
            for model in m.fallback.iter().flatten() {
                crate::config::check_model_name(model)
                    .map_err(|e| format!("machine {}: fallback: {e}", m.name))?;
            }
            crate::config::check_kind_agent_names(&m.agents)
                .map_err(|e| format!("machine {}: {e}", m.name))?;
            crate::harness::check_pins(&format!("machine {}: pins", m.name), &m.pins)?;
            crate::config::check_env(&format!("machine {}", m.name), &m.env)?;
            if let Some(p) = &m.profile {
                crate::config::check_profile_name(p)
                    .map_err(|e| format!("machine {}: {e}", m.name))?;
            }
            if let Some(p) = &m.provisioner {
                crate::connector::manifest::check_id(p)
                    .map_err(|e| format!("machine {}: provisioner {e}", m.name))?;
            }
            check_bundle_ids(&m.bundles)
                .map_err(|e| format!("machine {}: bundles: {e}", m.name))?;
            if m.max_agents == 0 {
                return Err(format!("machine {}: max_agents must be at least 1", m.name));
            }
            if m.reserved > m.max_agents {
                return Err(format!(
                    "machine {}: reserved {} is more than max_agents {}",
                    m.name, m.reserved, m.max_agents
                ));
            }
            for (key, size) in [("min_disk", &m.min_disk), ("min_memory", &m.min_memory)] {
                if let Some(v) = size {
                    crate::config::parse_size(v)
                        .map_err(|e| format!("machine {}: {key}: {e}", m.name))?;
                }
            }
            if let Some(f) = &m.flock
                && !self.has_flock(f)
            {
                return Err(format!("machine {}: flock {f} is not declared", m.name));
            }
            if let Some(f) = &m.flock
                && self
                    .entry(f)
                    .is_some_and(|e| e.machines.contains_key(&m.name))
            {
                return Err(format!(
                    "machine {}: in flock {f} twice, by its flock key and by the flock's machines",
                    m.name
                ));
            }
        }
        for f in &self.flocks {
            for (name, n) in &f.machines {
                if self.get(name).is_none() {
                    return Err(format!("flock {}: machine {name} is not declared", f.name));
                }
                if let Some(why) = n.problem() {
                    return Err(format!("flock {}: machine {name}: {why}", f.name));
                }
            }
        }
        Ok(())
    }

    /// Refuse a flock or machine `model` that `[models]` in pastor.toml does
    /// not define, an `agents` entry `check_kind_agents` refuses by the kinds
    /// `[agents]` gives, a `profile` that is neither built in nor in
    /// `[profiles]`, or a machine `provisioner` the catalog lacks. Apart from
    /// `validate` because models, agents and profiles live in the other
    /// file and provisioners in the data dir; every caller that loads both
    /// runs it.
    pub fn check_config(
        &self,
        models: &crate::config::Models,
        agents: &crate::config::Agents,
        profiles: &crate::config::profile::Profiles,
        provisioners: &crate::provision::Catalog,
    ) -> anyhow::Result<()> {
        for m in &self.machines {
            if let Some(p) = &m.provisioner {
                provisioners
                    .check(p)
                    .map_err(|e| anyhow::anyhow!("flock.toml: machine {}: {e}", m.name))?;
            }
        }
        let flocks = self.flocks.iter().map(|f| {
            (
                "flock",
                &f.name,
                &f.model,
                &f.fallback,
                &f.agent,
                &f.agents,
                &f.profile,
            )
        });
        let machines = self.machines.iter().map(|m| {
            (
                "machine",
                &m.name,
                &m.model,
                &m.fallback,
                &m.agent,
                &m.agents,
                &m.profile,
            )
        });
        for (what, name, model, fallback, agent, by_kind, profile) in flocks.chain(machines) {
            if let Some(model) = model {
                models
                    .check(model)
                    .map_err(|e| anyhow::anyhow!("flock.toml: {what} {name}: {e}"))?;
            }
            for model in fallback.iter().flatten() {
                models
                    .check(model)
                    .map_err(|e| anyhow::anyhow!("flock.toml: {what} {name}: fallback: {e}"))?;
            }
            crate::config::check_kind_agents(agent.as_deref(), by_kind, agents)
                .map_err(|e| anyhow::anyhow!("flock.toml: {what} {name}: {e}"))?;
            if let Some(profile) = profile {
                profiles
                    .resolve(profile)
                    .map_err(|e| anyhow::anyhow!("flock.toml: {what} {name}: {e}"))?;
            }
        }
        Ok(())
    }

    /// The flock tasks and jobs that name none go to.
    pub fn default_flock(&self) -> &str {
        self.flocks
            .iter()
            .find(|f| f.default)
            .map_or(DEFAULT_FLOCK, |f| f.name.as_str())
    }

    /// Every flock in file order; the implicit one when none is declared.
    pub fn flock_names(&self) -> Vec<&str> {
        if self.flocks.is_empty() {
            vec![DEFAULT_FLOCK]
        } else {
            self.flocks.iter().map(|f| f.name.as_str()).collect()
        }
    }

    /// The bundles `m` gets: its flocks' in file order, then its own, each
    /// once.
    pub fn bundles_of(&self, m: &MachineConfig) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let flocks = self.flocks_of(m);
        let from_flocks = flocks
            .iter()
            .filter_map(|(name, _)| self.entry(name))
            .flat_map(|f| &f.bundles);
        for id in from_flocks.chain(&m.bundles) {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }

    /// How `m` takes a new version of its bundles: its own `bundle_apply`,
    /// else its flocks' when they agree, else `manual`. Two flocks that
    /// differ give `manual`: automatic is never forced on a machine.
    pub fn bundle_apply(&self, m: &MachineConfig) -> crate::bundle::Apply {
        if let Some(a) = m.bundle_apply {
            return a;
        }
        let modes: Vec<crate::bundle::Apply> = self
            .flocks_of(m)
            .iter()
            .map(|(name, _)| {
                self.entry(name)
                    .and_then(|f| f.bundle_apply)
                    .unwrap_or_default()
            })
            .collect();
        match modes.split_first() {
            Some((first, rest)) if rest.iter().all(|a| a == first) => *first,
            _ => crate::bundle::Apply::Manual,
        }
    }

    /// The flocks and machines that name bundle `id` in flock.toml, and the
    /// machines that get it. A flock's `workdir_bundles` names it too; it
    /// reaches no machine's home.
    pub fn bundle_users(&self, id: &str) -> BundleUsers {
        let has = |list: &[String]| list.iter().any(|b| b == id);
        BundleUsers {
            flocks: self
                .flocks
                .iter()
                .filter(|f| has(&f.bundles) || has(&f.workdir_bundles))
                .map(|f| f.name.clone())
                .collect(),
            machines: self
                .machines
                .iter()
                .filter(|m| has(&m.bundles))
                .map(|m| m.name.clone())
                .collect(),
            affected: self
                .machines
                .iter()
                .filter(|m| has(&self.bundles_of(m)))
                .map(|m| m.name.clone())
                .collect(),
        }
    }

    /// Refuse a bundle that the bundles dir lacks or that does not load,
    /// and vars that do not fit the bundles: a flock must give every var
    /// of the bundles it names, and a machine every var of the bundles it
    /// gets from its flocks' values, which may not differ. A var no bundle
    /// declares is let be: it may be set before the version that needs it
    /// is added. Apart from `validate` because bundles live in the config
    /// dir; every caller that loads flock.toml for a head runs it.
    pub fn check_bundles(&self, bundles: &crate::bundle::Catalog) -> anyhow::Result<()> {
        let problems = self.bundle_problems(bundles);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("flock.toml: {}", problems.join("; ")))
        }
    }

    /// Each way `check_bundles` finds flock.toml does not fit `bundles`,
    /// as `flock F: ...` or `machine M: ...`.
    pub fn bundle_problems(&self, bundles: &crate::bundle::Catalog) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut get = |owner: String, id: &str| match bundles.check(id) {
            Ok(b) => Some(b.manifest.clone()),
            Err(e) => {
                let e = format!("{owner}: {e}");
                if !out.contains(&e) {
                    out.push(e);
                }
                None
            }
        };
        let mut found: Vec<String> = Vec::new();
        let home = crate::bundle::manifest::Target::Home;
        for f in &self.flocks {
            for id in &f.workdir_bundles {
                let Some(manifest) = get(format!("flock {}", f.name), id) else {
                    continue;
                };
                if manifest.target() == home {
                    found.push(format!(
                        "flock {}: bundle {id} is a home bundle: name it in bundles, not workdir_bundles",
                        f.name
                    ));
                    continue;
                }
                for var in manifest
                    .var_names()
                    .filter(|v| !f.bundle_vars.contains_key(*v))
                {
                    found.push(format!(
                        "flock {}: bundle {id} needs bundle_vars {var}",
                        f.name
                    ));
                }
            }
            for id in &f.bundles {
                let Some(manifest) = get(format!("flock {}", f.name), id) else {
                    continue;
                };
                if manifest.target() != home {
                    found.push(format!(
                        "flock {}: bundle {id} goes into tasks' working directories: name it in workdir_bundles, not bundles",
                        f.name
                    ));
                    continue;
                }
                for var in manifest
                    .var_names()
                    .filter(|v| !f.bundle_vars.contains_key(*v))
                {
                    found.push(format!(
                        "flock {}: bundle {id} needs bundle_vars {var}",
                        f.name
                    ));
                }
            }
        }
        for m in &self.machines {
            let mut values: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
            for (name, _) in self.flocks_of(m) {
                let Some(f) = self.entry(name) else { continue };
                for (k, v) in &f.bundle_vars {
                    match values.get(k.as_str()) {
                        Some((other, w)) if w != v => found.push(format!(
                            "machine {}: bundle_vars {k} is {w:?} in flock {other} and {v:?} in flock {name}",
                            m.name
                        )),
                        Some(_) => {}
                        None => {
                            values.insert(k, (name, v));
                        }
                    }
                }
            }
            for id in &m.bundles {
                let Some(manifest) = get(format!("machine {}", m.name), id) else {
                    continue;
                };
                if manifest.target() != home {
                    found.push(format!(
                        "machine {}: bundle {id} goes into tasks' working directories: name it in a flock's workdir_bundles",
                        m.name
                    ));
                    continue;
                }
                for var in manifest.var_names().filter(|v| !values.contains_key(v)) {
                    found.push(format!(
                        "machine {}: bundle {id} needs bundle_vars {var}, in one of its flocks",
                        m.name
                    ));
                }
            }
        }
        out.extend(found);
        out
    }

    /// The `[[flock]]` entry named `name`; `None` for the implicit flock of
    /// a file that declares none, which has no settings.
    pub fn entry(&self, name: &str) -> Option<&FlockEntry> {
        self.flocks.iter().find(|f| f.name == name)
    }

    pub fn has_flock(&self, name: &str) -> bool {
        self.flock_names().contains(&name)
    }

    /// The flocks `m` is in, in file order, each with its number there: at
    /// most how many of that flock's live tasks `m` runs. A flock whose
    /// `machines` lists `m` counts with the number it gives, and so does the
    /// flock its old `flock` key names, with `None`: the machine's own
    /// limits, `max_agents` and on top of it the job slots and burst. A
    /// machine neither places is in the default flock the same way, as
    /// every machine was before flocks.
    pub fn flocks_of<'a>(&'a self, m: &MachineConfig) -> Vec<(&'a str, Option<FlockNumber>)> {
        let mut out: Vec<(&str, Option<FlockNumber>)> = self
            .flocks
            .iter()
            .filter_map(|f| {
                if let Some(n) = f.machines.get(&m.name) {
                    Some((f.name.as_str(), Some(*n)))
                } else if m.flock.as_deref() == Some(f.name.as_str()) {
                    Some((f.name.as_str(), None))
                } else {
                    None
                }
            })
            .collect();
        if out.is_empty() {
            out.push((self.default_flock(), None));
        }
        out
    }

    /// `flocks_of` the machine named `name`, `None` when there is no such
    /// machine.
    pub fn machine_flocks(&self, name: &str) -> Option<Vec<(&str, Option<FlockNumber>)>> {
        self.get(name).map(|m| self.flocks_of(m))
    }

    /// The loads `m` takes: its own `loads`, then those of each flock it
    /// is in, each once. A task with a load is placed only on a machine
    /// whose list has it, whatever flock the task is in.
    pub fn loads_of(&self, m: &MachineConfig) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let flocks = self.flocks_of(m);
        let theirs = flocks
            .iter()
            .filter_map(|(f, _)| self.entry(f))
            .flat_map(|f| &f.loads);
        for load in m.loads.iter().chain(theirs) {
            if !out.contains(load) {
                out.push(load.clone());
            }
        }
        out
    }

    /// The machines of `flock` that take `load`, in file order.
    pub fn takers(&self, flock: &str, load: &str) -> Vec<&str> {
        self.machines
            .iter()
            .filter(|m| self.in_flock(m, flock) && self.loads_of(m).iter().any(|l| l == load))
            .map(|m| m.name.as_str())
            .collect()
    }

    /// Whether `m` is in `flock`.
    pub fn in_flock(&self, m: &MachineConfig, flock: &str) -> bool {
        self.flocks_of(m).iter().any(|(f, _)| *f == flock)
    }

    /// The machines in `flock`, in file order.
    pub fn members(&self, flock: &str) -> Vec<&str> {
        self.machines
            .iter()
            .filter(|m| self.in_flock(m, flock))
            .map(|m| m.name.as_str())
            .collect()
    }

    /// Whether nothing but the default puts `m` in a flock: no `flock` key
    /// and no flock's `machines`. Such a machine follows the default.
    pub fn unplaced(&self, m: &MachineConfig) -> bool {
        m.flock.is_none()
            && self
                .flocks
                .iter()
                .all(|f| !f.machines.contains_key(&m.name))
    }

    /// The harness pins that hold on `m`: its own over its primary flock's,
    /// per kind.
    pub fn pins_for(&self, m: &MachineConfig) -> crate::harness::Pins {
        let mut pins = self
            .entry(self.primary_flock(m))
            .map(|f| f.pins.clone())
            .unwrap_or_default();
        pins.extend(m.pins.clone());
        pins
    }

    /// The one flock that stands for `m` where one name is wanted (a task
    /// pinned to it that names none, a machine's profile, an old reader):
    /// the default flock when `m` is in it, else its first.
    pub fn primary_flock<'a>(&'a self, m: &MachineConfig) -> &'a str {
        let flocks = self.flocks_of(m);
        let default = self.default_flock();
        if flocks.iter().any(|(f, _)| *f == default) {
            default
        } else {
            flocks[0].0
        }
    }

    /// `primary_flock` of the machine named `name`, `None` when there is no
    /// such machine.
    pub fn machine_flock(&self, name: &str) -> Option<&str> {
        self.get(name).map(|m| self.primary_flock(m))
    }

    /// The flock a new task targets: the one it names, else the primary
    /// flock of the machine it is pinned to, else the default. A named flock
    /// must exist, and a pinned machine must be in it. A pin to a machine this file does
    /// not have settles nothing: the caller refuses that machine itself.
    pub fn task_flock(
        &self,
        requested: Option<&str>,
        pinned: Option<&str>,
    ) -> Result<String, TaskFlockError> {
        if let Some(f) = requested
            && !self.has_flock(f)
        {
            return Err(TaskFlockError::UnknownFlock(f.to_string()));
        }
        let pinned = pinned.and_then(|m| self.get(m));
        match (requested, pinned) {
            (Some(want), Some(m)) if !self.in_flock(m, want) => {
                Err(TaskFlockError::MachineElsewhere {
                    machine: m.name.clone(),
                    flocks: self
                        .flocks_of(m)
                        .into_iter()
                        .map(|(f, _)| f.to_string())
                        .collect(),
                    requested: want.to_string(),
                })
            }
            (Some(want), _) => Ok(want.to_string()),
            (None, Some(m)) => Ok(self.primary_flock(m).to_string()),
            (None, None) => Ok(self.default_flock().to_string()),
        }
    }

    pub fn get(&self, name: &str) -> Option<&MachineConfig> {
        self.machines.iter().find(|m| m.name == name)
    }

    pub fn add(&mut self, m: MachineConfig) -> Result<(), String> {
        if self.get(&m.name).is_some() {
            return Err(format!("machine {} already exists", m.name));
        }
        self.machines.push(m);
        if let Err(e) = self.validate() {
            self.machines.pop();
            return Err(e);
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.machines.len();
        self.machines.retain(|m| m.name != name);
        self.machines.len() != before
    }
}

/// Why an edit of flock.toml was refused. Each has a stable CLI code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("machine {0} already exists")]
    MachineExists(String),
    #[error("machine {0} not found")]
    UnknownMachine(String),
    #[error("machine name {0:?} is refused: {MACHINE_NAME_RULE}")]
    InvalidMachineName(String),
    #[error("flock {0} already exists")]
    FlockExists(String),
    #[error("flock {0} does not exist")]
    UnknownFlock(String),
    #[error("flock {flock} still has machines: {}; move them first", machines.join(", "))]
    FlockHasMachines {
        flock: String,
        machines: Vec<String>,
    },
    #[error("flock {flock} still has queued tasks: {}; close them first", tasks.join(", "))]
    FlockHasTasks { flock: String, tasks: Vec<String> },
    #[error("flock {0} is the default; make another flock the default first")]
    RemovingDefault(String),
    #[error("machine {machine} is not in flock {flock}")]
    NotInFlock { machine: String, flock: String },
    #[error(
        "machine {machine} is in flock {flock} only because no flock lists it; join it to another flock instead"
    )]
    Unlisted { machine: String, flock: String },
    #[error("{0}")]
    Invalid(String),
}

impl EditError {
    pub fn code(&self) -> &'static str {
        match self {
            EditError::MachineExists(_) => "machine_exists",
            EditError::UnknownMachine(_) => "unknown_machine",
            EditError::InvalidMachineName(_) => "invalid_machine",
            EditError::FlockExists(_) => "flock_exists",
            EditError::UnknownFlock(_) => "unknown_flock",
            EditError::FlockHasMachines { .. } => "flock_not_empty",
            EditError::FlockHasTasks { .. } => "flock_has_tasks",
            EditError::RemovingDefault(_) => "flock_is_default",
            EditError::NotInFlock { .. } | EditError::Unlisted { .. } => "not_in_flock",
            EditError::Invalid(_) => "config_error",
        }
    }
}

/// What `flock add` did with the machines that name no flock of their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlockAdded {
    /// The flock they are in after the edit.
    pub flock: String,
    pub machines: Vec<String>,
    /// They followed the new flock, rather than staying where they were.
    pub moved: bool,
    /// The queued tasks in the implicit flock that kept them from following
    /// a new default: those tasks would be left waiting in a flock with no
    /// machines.
    pub held_by: Vec<String>,
}

/// flock.toml opened for an edit that keeps everything it does not touch:
/// comments, key order, blank lines. `machine add|remove|move` and the
/// `flock` commands go through it; `Flock::save` would rewrite the file from
/// scratch. Each edit checks what it needs against the file as it stands,
/// and `save` checks the result loads before it replaces the file.
pub struct FlockDoc {
    doc: toml_edit::DocumentMut,
}

impl FlockDoc {
    /// A missing file is an empty one, as for `Flock::load`. A file that
    /// does not load is refused: an edit must not paper over it.
    pub fn open(path: &Path) -> anyhow::Result<FlockDoc> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let doc: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("parse {}", path.display()))?;
        let d = FlockDoc { doc };
        d.flock()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(d)
    }

    pub fn parse(text: &str) -> anyhow::Result<FlockDoc> {
        let d = FlockDoc { doc: text.parse()? };
        d.flock().map_err(anyhow::Error::msg)?;
        Ok(d)
    }

    /// The file as it now reads, validated.
    pub fn flock(&self) -> Result<Flock, String> {
        let f: Flock = toml::from_str(&self.doc.to_string()).map_err(|e| e.to_string())?;
        f.validate()?;
        Ok(f)
    }

    /// Write the edited file in place of `path` (a temp file, then a rename,
    /// so a reader never sees half of it). Refused if it would not load.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.flock().map_err(anyhow::Error::msg)?;
        if let Some(parent) = path.parent() {
            crate::config::create_private_dir(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_string())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    fn current(&self) -> Result<Flock, EditError> {
        self.flock().map_err(EditError::Invalid)
    }

    /// Every table in the document with its position, for placing new ones.
    fn positions(&self) -> Vec<(bool, isize)> {
        let mut out = Vec::new();
        for (key, item) in self.doc.as_table().iter() {
            let is_flock = key == "flock";
            match item {
                toml_edit::Item::ArrayOfTables(a) => {
                    out.extend(a.iter().filter_map(|t| Some((is_flock, t.position()?))))
                }
                toml_edit::Item::Table(t) => out.extend(t.position().map(|p| (is_flock, p))),
                _ => {}
            }
        }
        out
    }

    fn array(&mut self, key: &str) -> &mut toml_edit::ArrayOfTables {
        let item = self
            .doc
            .as_table_mut()
            .entry(key)
            .or_insert_with(|| toml_edit::Item::ArrayOfTables(Default::default()));
        item.as_array_of_tables_mut()
            .expect("checked by flock(): the key holds an array of tables")
    }

    /// A new `[[<key>]]` table: flocks after the last flock (or before
    /// everything when there is none, so they head the file), machines at
    /// the end. Separated from what comes before by a blank line.
    fn push(&mut self, key: &str, mut table: toml_edit::Table) {
        let pos = self.positions();
        let at = if key == "flock" {
            pos.iter()
                .filter(|(f, _)| *f)
                .map(|(_, p)| *p)
                .max()
                .unwrap_or_else(|| pos.iter().map(|(_, p)| *p).min().unwrap_or(0) - 1)
        } else {
            pos.iter().map(|(_, p)| *p).max().unwrap_or(0) + 1
        };
        table.set_position(Some(at));
        let first = pos.iter().all(|(_, p)| *p > at);
        let blank = self.doc.to_string().trim().is_empty();
        let prefix = match self.first_table_mut() {
            // The new table heads the file, so the file's header comment
            // (the old first table's prefix, up to its last blank line)
            // moves up with it; what follows the blank line stays with the
            // table it was written above.
            Some(old) if first => {
                let p = old.decor().prefix().and_then(|p| p.as_str()).unwrap_or("");
                let (header, rest) = match p.rfind("\n\n") {
                    Some(i) => (format!("{}\n", &p[..=i]), p[i + 2..].to_string()),
                    None => (String::new(), p.to_string()),
                };
                old.decor_mut().set_prefix(format!("\n{rest}"));
                header
            }
            Some(_) => "\n".to_string(),
            None if blank => String::new(),
            None => "\n".to_string(),
        };
        table.decor_mut().set_prefix(prefix);
        self.array(key).push(table);
    }

    /// The table that comes first in the file.
    fn first_table_mut(&mut self) -> Option<&mut toml_edit::Table> {
        let mut tables: Vec<&mut toml_edit::Table> = Vec::new();
        for (_, item) in self.doc.as_table_mut().iter_mut() {
            match item {
                toml_edit::Item::ArrayOfTables(a) => tables.extend(a.iter_mut()),
                toml_edit::Item::Table(t) => tables.push(t),
                _ => {}
            }
        }
        tables
            .into_iter()
            .filter(|t| t.position().is_some())
            .min_by_key(|t| t.position())
    }

    fn tables_mut<'a>(
        &'a mut self,
        key: &str,
    ) -> impl Iterator<Item = &'a mut toml_edit::Table> + 'a {
        self.doc
            .get_mut(key)
            .and_then(|i| i.as_array_of_tables_mut())
            .into_iter()
            .flat_map(|a| a.iter_mut())
    }

    fn machine_mut(&mut self, name: &str) -> Option<&mut toml_edit::Table> {
        self.tables_mut("machine")
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
    }

    fn remove_named(&mut self, key: &str, name: &str) {
        if let Some(a) = self
            .doc
            .get_mut(key)
            .and_then(|i| i.as_array_of_tables_mut())
        {
            a.retain(|t| t.get("name").and_then(|v| v.as_str()) != Some(name));
        }
    }

    /// With no `[[flock]]` entry the file has one implicit flock; naming a
    /// second one needs the first on paper, as the default. A no-op once
    /// the document has one.
    fn declare_implicit(&mut self) {
        if self.tables_mut("flock").next().is_none() {
            let mut t = toml_edit::Table::new();
            t.insert("name", toml_edit::value(DEFAULT_FLOCK));
            t.insert("default", toml_edit::value(true));
            self.push("flock", t);
        }
    }

    pub fn add_machine(&mut self, m: &MachineConfig) -> Result<(), EditError> {
        if !crate::authkeys::valid_name(&m.name) {
            return Err(EditError::InvalidMachineName(m.name.clone()));
        }
        let f = self.current()?;
        if f.get(&m.name).is_some() {
            return Err(EditError::MachineExists(m.name.clone()));
        }
        if let Some(name) = &m.flock
            && !f.has_flock(name)
        {
            return Err(EditError::UnknownFlock(name.clone()));
        }
        let mut probe = f.clone();
        probe.machines.push(m.clone());
        probe.validate().map_err(EditError::Invalid)?;
        let text = toml::to_string(m).map_err(|e| EditError::Invalid(e.to_string()))?;
        let table: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| EditError::Invalid(e.to_string()))?;
        self.push("machine", table.as_table().clone());
        Ok(())
    }

    pub fn remove_machine(&mut self, name: &str) -> Result<(), EditError> {
        if self.current()?.get(name).is_none() {
            return Err(EditError::UnknownMachine(name.into()));
        }
        self.remove_named("machine", name);
        for t in self.tables_mut("flock") {
            if let Some(ms) = t.get_mut("machines").and_then(|i| i.as_table_like_mut()) {
                ms.remove(name);
            }
        }
        Ok(())
    }

    /// The `[[flock]]` table named `name`.
    fn flock_mut(&mut self, name: &str) -> Option<&mut toml_edit::Table> {
        self.tables_mut("flock")
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
    }

    /// Set `machine`'s number in `flock`'s `machines`, writing the table
    /// inline (`machines = { desk = 2 }`) when the flock has none.
    fn set_number(&mut self, flock: &str, machine: &str, n: FlockNumber) {
        let t = self.flock_mut(flock).expect("declared by the caller");
        let ms = t
            .entry("machines")
            .or_insert_with(|| toml_edit::value(toml_edit::InlineTable::new()));
        let value = match n {
            FlockNumber::Plain(n) => toml_edit::value(i64::from(n)),
            FlockNumber::Split(_) => {
                let mut split = toml_edit::InlineTable::new();
                split.insert("share", i64::from(n.share()).into());
                split.insert("max", i64::from(n.max()).into());
                toml_edit::value(split)
            }
        };
        ms.as_table_like_mut()
            .expect("checked by flock(): machines is a table")
            .insert(machine, value);
    }

    /// Take `machine` out of `flock`'s `machines`, and the table with it
    /// once it is empty.
    fn unset_number(&mut self, flock: &str, machine: &str) {
        let Some(t) = self.flock_mut(flock) else {
            return;
        };
        let empty = match t.get_mut("machines").and_then(|i| i.as_table_like_mut()) {
            Some(ms) => {
                ms.remove(machine);
                ms.is_empty()
            }
            None => false,
        };
        if empty {
            t.remove("machines");
        }
    }

    /// The first membership edit of `m`: its old `flock` key becomes an
    /// entry in that flock's `machines` with the machine's `max_agents`, the
    /// number the key stood for. Nothing else in the file moves.
    fn lift_flock_key(&mut self, m: &MachineConfig) {
        let Some(old) = &m.flock else {
            return;
        };
        self.declare_implicit();
        let t = self.machine_mut(&m.name).expect("listed by current()");
        t.remove("flock");
        self.set_number(old, &m.name, FlockNumber::plain(m.max_agents));
    }

    /// `flock join`: list `machine` in `flock` with `max`, a plain number,
    /// by default the number it has there already (a share and a max
    /// stay), else its `max_agents`. Returns the number written.
    pub fn join_flock(
        &mut self,
        machine: &str,
        flock: &str,
        max: Option<u32>,
    ) -> Result<FlockNumber, EditError> {
        let f = self.current()?;
        let m = f
            .get(machine)
            .cloned()
            .ok_or_else(|| EditError::UnknownMachine(machine.into()))?;
        if !f.has_flock(flock) {
            return Err(EditError::UnknownFlock(flock.into()));
        }
        let listed = f
            .entry(flock)
            .and_then(|e| e.machines.get(machine).copied());
        let n = max
            .map(FlockNumber::plain)
            .or(listed)
            .unwrap_or(FlockNumber::plain(m.max_agents));
        self.declare_implicit();
        self.lift_flock_key(&m);
        self.set_number(flock, machine, n);
        Ok(n)
    }

    /// `flock leave`: take `machine` out of `flock`. Out of its last one it
    /// is in the default flock again, as a machine no flock lists. A machine
    /// in the default only for that has nothing to leave.
    pub fn leave_flock(&mut self, machine: &str, flock: &str) -> Result<(), EditError> {
        let f = self.current()?;
        let m = f
            .get(machine)
            .cloned()
            .ok_or_else(|| EditError::UnknownMachine(machine.into()))?;
        if !f.has_flock(flock) {
            return Err(EditError::UnknownFlock(flock.into()));
        }
        if !f.in_flock(&m, flock) {
            return Err(EditError::NotInFlock {
                machine: machine.into(),
                flock: flock.into(),
            });
        }
        if f.unplaced(&m) {
            return Err(EditError::Unlisted {
                machine: machine.into(),
                flock: flock.into(),
            });
        }
        self.lift_flock_key(&m);
        self.unset_number(flock, machine);
        Ok(())
    }

    /// `machine move`: leave every flock and join `flock` with the machine's
    /// `max_agents`, listed by name, so it stays there whichever flock is
    /// the default later. A no-op when `flock`'s `machines` is already its
    /// only membership; a machine with the old `flock` key is edited even
    /// when the key names `flock`, since this is its first membership edit.
    pub fn move_machine(&mut self, name: &str, flock: &str) -> Result<(), EditError> {
        let f = self.current()?;
        let m = f
            .get(name)
            .cloned()
            .ok_or_else(|| EditError::UnknownMachine(name.into()))?;
        if !f.has_flock(flock) {
            return Err(EditError::UnknownFlock(flock.into()));
        }
        let now = f.flocks_of(&m);
        if m.flock.is_none() && matches!(now.as_slice(), [(only, Some(_))] if *only == flock) {
            return Ok(());
        }
        self.declare_implicit();
        self.machine_mut(name)
            .expect("checked above")
            .remove("flock");
        for (other, _) in &now {
            self.unset_number(other, name);
        }
        self.set_number(flock, name, FlockNumber::plain(m.max_agents));
        Ok(())
    }

    /// `flock add`. A file with only the implicit flock gets it declared
    /// first, so its machines keep their flock; unless the new flock is to
    /// be the default, which then takes the implicit one's place and its
    /// machines. The implicit flock is still declared when a machine names
    /// it, and that machine stays in it. `queued` holds the ids of the queued
    /// tasks in the implicit flock; while there are any the machines stay,
    /// since dispatch only looks in declared flocks and those tasks would
    /// wait forever.
    pub fn add_flock(
        &mut self,
        name: &str,
        default: bool,
        queued: &[String],
    ) -> Result<FlockAdded, EditError> {
        let f = self.current()?;
        if f.has_flock(name) {
            return Err(EditError::FlockExists(name.into()));
        }
        if name.is_empty() {
            return Err(EditError::Invalid("flock with empty name".into()));
        }
        let machines: Vec<String> = f
            .machines
            .iter()
            .filter(|m| f.unplaced(m))
            .map(|m| m.name.clone())
            .collect();
        if default && f.flocks.is_empty() && queued.is_empty() {
            if f.machines.iter().any(|m| m.flock.is_some()) {
                let mut t = toml_edit::Table::new();
                t.insert("name", toml_edit::value(DEFAULT_FLOCK));
                self.push("flock", t);
            }
            let mut t = toml_edit::Table::new();
            t.insert("name", toml_edit::value(name));
            t.insert("default", toml_edit::value(true));
            self.push("flock", t);
            return Ok(FlockAdded {
                flock: name.into(),
                machines,
                moved: true,
                held_by: Vec::new(),
            });
        }
        self.declare_implicit();
        let mut t = toml_edit::Table::new();
        t.insert("name", toml_edit::value(name));
        self.push("flock", t);
        if default {
            self.set_default(name)?;
        }
        let held_by = if default && f.flocks.is_empty() && !machines.is_empty() {
            queued.to_vec()
        } else {
            Vec::new()
        };
        Ok(FlockAdded {
            flock: f.default_flock().into(),
            machines,
            moved: false,
            held_by,
        })
    }

    /// `flock add --description`: set the description of the declared flock
    /// `name`.
    pub fn describe_flock(&mut self, name: &str, text: &str) -> Result<(), EditError> {
        let t = self
            .flock_mut(name)
            .ok_or_else(|| EditError::UnknownFlock(name.into()))?;
        t.insert("description", toml_edit::value(text));
        Ok(())
    }

    /// `flock remove`: refused while machines are in it, while `queued`
    /// (the ids of the queued tasks that name it) is not empty, or while it
    /// is the default. Dispatch only looks in declared flocks, so a queued
    /// task left naming a removed one would wait forever.
    pub fn remove_flock(&mut self, name: &str, queued: &[String]) -> Result<(), EditError> {
        let f = self.current()?;
        // `has_flock` counts the implicit `default` of a file with no
        // `[[flock]]`, so removing it reads as removing the default.
        if !f.has_flock(name) {
            return Err(EditError::UnknownFlock(name.into()));
        }
        if f.default_flock() == name {
            return Err(EditError::RemovingDefault(name.into()));
        }
        let machines: Vec<String> = f.members(name).into_iter().map(String::from).collect();
        if !machines.is_empty() {
            return Err(EditError::FlockHasMachines {
                flock: name.into(),
                machines,
            });
        }
        if !queued.is_empty() {
            return Err(EditError::FlockHasTasks {
                flock: name.into(),
                tasks: queued.to_vec(),
            });
        }
        self.remove_named("flock", name);
        Ok(())
    }

    /// `flock default`: tasks and jobs that name no flock go to `name` from
    /// now on. A machine with no `flock` of its own is in the default flock,
    /// so each one is first written into the flock it is in now: changing
    /// where new work goes must not move machines.
    pub fn set_default(&mut self, name: &str) -> Result<(), EditError> {
        let f = self.current()?;
        if !f.has_flock(name) {
            return Err(EditError::UnknownFlock(name.into()));
        }
        let old = f.default_flock().to_string();
        if old == name {
            return Ok(());
        }
        self.declare_implicit();
        for m in f.machines.iter().filter(|m| f.unplaced(m)) {
            let t = self.machine_mut(&m.name).expect("listed by current()");
            t.insert("flock", toml_edit::value(old.as_str()));
        }
        for t in self.tables_mut("flock") {
            if t.get("name").and_then(|v| v.as_str()) == Some(name) {
                t.insert("default", toml_edit::value(true));
            } else {
                t.remove("default");
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for FlockDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.doc.fmt(f)
    }
}

/// Why `target`, the `ssh` of a machine, is not one plain destination. ssh
/// parses a target that starts with `-` as an option (`-oProxyCommand=...`
/// runs a local command), and pastor passes it after `--` as well; spaces and
/// control characters have no place in a `[user@]host`.
pub(crate) fn ssh_target_problem(target: &str) -> Option<&'static str> {
    if target.is_empty() {
        Some("is empty")
    } else if target.starts_with('-') {
        Some("starts with '-'")
    } else if target.chars().any(char::is_control) {
        Some("contains a control character")
    } else if target.chars().any(char::is_whitespace) {
        Some("contains whitespace")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A misspelt key in flock.toml fails the load, naming the file and
    /// the key.
    fn flock_typo_error(text: &str) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        std::fs::write(&path, text).unwrap();
        // Display, not `{:#}`: the reload logs only the top of the error.
        let err = Flock::load(&path).unwrap_err().to_string();
        assert!(err.contains("flock.toml"), "{err}");
        assert!(Flock::load_existing(&path).is_err());
        err
    }

    #[test]
    fn a_typo_in_a_machine_is_a_load_error() {
        let err = flock_typo_error("[[machine]]\nname = \"m\"\nlocal = true\nmax_agent = 1\n");
        assert!(err.contains("max_agent"), "{err}");
    }

    #[test]
    fn a_typo_in_a_top_level_key_is_a_load_error() {
        let err = flock_typo_error("[[machines]]\nname = \"m\"\nlocal = true\n");
        assert!(err.contains("machines"), "{err}");
    }

    /// The legacy `flock = "..."` on a machine still loads, and so does
    /// what `machine add` and `flock join` write.
    #[test]
    fn legacy_and_written_machines_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        std::fs::write(
            &path,
            "[[flock]]\nname = \"a\"\ndefault = true\n[[machine]]\nname = \"m\"\nlocal = true\nflock = \"a\"\n",
        )
        .unwrap();
        assert_eq!(
            Flock::load(&path).unwrap().machines[0].flock.as_deref(),
            Some("a")
        );
        let mut doc = FlockDoc::open(&path).unwrap();
        doc.add_flock("b", false, &[]).unwrap();
        doc.add_machine(&MachineConfig {
            reserved: 0,
            provisioner: None,
            bundles: Vec::new(),
            bundle_apply: None,
            name: "pi".into(),
            local: false,
            pull: false,
            ssh: Some("user@pi-1".into()),
            command: None,
            session: "default".into(),
            max_agents: 3,
            job_slots: 0,
            burst: 2,
            tags: vec!["arm".into()],
            loads: vec![],
            flock: Some("a".into()),
            agent: Some("codex".into()),
            agent_args: Some(vec![]),
            model: None,
            fallback: None,
            priority: Some(crate::task::Priority::High),
            agents: Default::default(),
            profile: None,
            min_disk: None,
            min_memory: None,
            unknown_room: None,
            description: Some("the pi".into()),
            pins: Default::default(),
            env: Default::default(),
        })
        .unwrap();
        doc.join_flock("pi", "b", None).unwrap();
        doc.join_flock("m", "b", Some(1)).unwrap();
        doc.save(&path).unwrap();
        let f = Flock::load(&path).unwrap();
        assert!(f.in_flock(f.get("pi").unwrap(), "b"));
        assert!(f.in_flock(f.get("m").unwrap(), "b"));
    }

    fn pi(name: &str) -> MachineConfig {
        MachineConfig {
            reserved: 0,
            provisioner: None,
            bundles: Vec::new(),
            bundle_apply: None,
            pull: false,
            description: None,
            name: name.into(),
            local: false,
            ssh: Some(format!("fleet@{name}")),
            command: None,
            session: "default".into(),
            max_agents: 2,
            job_slots: 1,
            burst: 1,
            tags: vec![],
            loads: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            fallback: None,
            priority: None,
            agents: Default::default(),
            profile: None,
            min_disk: None,
            min_memory: None,
            unknown_room: None,
            pins: Default::default(),
            env: Default::default(),
        }
    }

    fn flocks(text: &str) -> Result<Flock, String> {
        let f: Flock = toml::from_str(text).map_err(|e| e.to_string())?;
        f.validate()?;
        Ok(f)
    }

    /// `job_slots` and `burst` default to 1, `0` is allowed, and a file
    /// that leaves them at 1 does not write them.
    #[test]
    fn job_slots_and_burst_default_to_one() {
        let f: Flock = toml::from_str(
            "[[machine]]\nname = \"a\"\nlocal = true\n\n[[machine]]\nname = \"b\"\nlocal = true\njob_slots = 0\nburst = 2\n",
        )
        .unwrap();
        f.validate().unwrap();
        assert_eq!((f.machines[0].job_slots, f.machines[0].burst), (1, 1));
        assert_eq!((f.machines[1].job_slots, f.machines[1].burst), (0, 2));
        let out = toml::to_string(&f).unwrap();
        assert!(
            out.contains("job_slots = 0") && out.contains("burst = 2"),
            "{out}"
        );
        assert_eq!(out.matches("burst").count(), 1, "{out}");
    }

    /// A machine's `min_disk`, `min_memory` and `unknown_room` load as
    /// written, a flock's `unknown_room` too, and a file that sets none
    /// writes none: the floor stays off.
    #[test]
    fn floor_keys_load_and_are_off_when_unset() {
        let f = flocks(
            "[[flock]]\nname = \"f\"\ndefault = true\nunknown_room = \"no_room\"\n\n[[machine]]\nname = \"a\"\nlocal = true\n\n[[machine]]\nname = \"b\"\nlocal = true\nmin_disk = \"4G\"\nmin_memory = \"0\"\nunknown_room = \"no_room\"\n",
        )
        .unwrap();
        assert_eq!(
            f.flocks[0].unknown_room,
            Some(crate::config::UnknownRoom::NoRoom)
        );
        let a = &f.machines[0];
        assert_eq!(
            (&a.min_disk, &a.min_memory, a.unknown_room),
            (&None, &None, None)
        );
        let b = &f.machines[1];
        assert_eq!(b.min_disk.as_deref(), Some("4G"));
        assert_eq!(b.min_memory.as_deref(), Some("0"));
        assert_eq!(b.unknown_room, Some(crate::config::UnknownRoom::NoRoom));
        let out = toml::to_string(&f).unwrap();
        assert_eq!(out.matches("min_disk").count(), 1, "{out}");
        assert_eq!(out.matches("unknown_room").count(), 2, "{out}");
    }

    /// A floor size that does not parse, or an `unknown_room` that is
    /// neither `room` nor `no_room`, is a load error naming the key.
    #[test]
    fn bad_floor_keys_are_load_errors() {
        let path = Path::new("flock.toml");
        for (key, bad) in [
            ("min_disk", "4"),
            ("min_disk", "four"),
            ("min_memory", "1x"),
            ("min_memory", ""),
        ] {
            let text = format!("[[machine]]\nname = \"a\"\nlocal = true\n{key} = {bad:?}\n");
            let err = Flock::parse(path, &text).unwrap_err().to_string();
            assert!(
                err.contains(&format!("machine a: {key}")),
                "{key} = {bad:?}: {err}"
            );
        }
        let err = Flock::parse(
            path,
            "[[machine]]\nname = \"a\"\nlocal = true\nunknown_room = \"maybe\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown_room"), "{err}");
        let err = Flock::parse(
            path,
            "[[flock]]\nname = \"f\"\ndefault = true\nunknown_room = true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown_room"), "{err}");
    }

    #[test]
    fn parses_the_flocks_of_the_spec_example() {
        let f = flocks(
            r#"
[[flock]]
name = "personal"
default = true

[[flock]]
name = "work"

[[machine]]
name = "pi-1"
local = true

[[machine]]
name = "pi-3"
ssh = "user@pi-3"
flock = "work"
"#,
        )
        .unwrap();
        assert_eq!(f.default_flock(), "personal");
        assert_eq!(f.flock_names(), ["personal", "work"]);
        assert_eq!(f.machine_flock("pi-1"), Some("personal"));
        assert_eq!(f.machine_flock("pi-3"), Some("work"));
        assert_eq!(f.machine_flock("pi-9"), None);
        assert!(f.has_flock("work"));
        assert!(!f.has_flock("default"));
    }

    /// Flock files from before flocks keep working: one flock, `default`.
    #[test]
    fn no_flock_entry_is_one_flock_named_default() {
        let f = flocks("[[machine]]\nname = \"a\"\nlocal = true\n").unwrap();
        assert_eq!(f.default_flock(), DEFAULT_FLOCK);
        assert_eq!(f.flock_names(), [DEFAULT_FLOCK]);
        assert_eq!(f.machine_flock("a"), Some(DEFAULT_FLOCK));
        // Naming the implicit flock is allowed; any other name is not.
        flocks("[[machine]]\nname = \"a\"\nlocal = true\nflock = \"default\"\n").unwrap();
        let err =
            flocks("[[machine]]\nname = \"a\"\nlocal = true\nflock = \"work\"\n").unwrap_err();
        assert_eq!(err, "machine a: flock work is not declared");
        assert_eq!(Flock::default().default_flock(), DEFAULT_FLOCK);
    }

    #[test]
    fn flock_errors_are_config_errors() {
        let two =
            "[[flock]]\nname = \"a\"\ndefault = true\n[[flock]]\nname = \"b\"\ndefault = true\n";
        assert_eq!(
            flocks(two).unwrap_err(),
            "flocks a and b are both default; exactly one may be"
        );
        let none = "[[flock]]\nname = \"a\"\n[[flock]]\nname = \"b\"\n";
        assert_eq!(
            flocks(none).unwrap_err(),
            "no flock has default = true; exactly one must"
        );
        let dup = "[[flock]]\nname = \"a\"\ndefault = true\n[[flock]]\nname = \"a\"\n";
        assert_eq!(flocks(dup).unwrap_err(), "flock a listed twice");
        let empty = "[[flock]]\nname = \"\"\ndefault = true\n";
        assert_eq!(flocks(empty).unwrap_err(), "flock with empty name");
        let unknown = "[[flock]]\nname = \"a\"\ndefault = true\n[[machine]]\nname = \"m\"\nlocal = true\nflock = \"b\"\n";
        assert_eq!(
            flocks(unknown).unwrap_err(),
            "machine m: flock b is not declared"
        );
        // A typo in a key is a load error too, not a machine in the default flock.
        assert!(flocks("[[flock]]\nname = \"a\"\ndefualt = true\n").is_err());
    }

    #[test]
    fn a_bad_flock_fails_the_load_with_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        std::fs::write(&path, "[[flock]]\nname = \"a\"\n").unwrap();
        let err = format!("{:#}", Flock::load(&path).unwrap_err());
        assert!(err.contains("flock.toml"), "{err}");
        assert!(err.contains("no flock has default = true"), "{err}");
        assert!(Flock::load_existing(&path).is_err());
    }

    #[test]
    fn flocks_survive_a_save() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let f = flocks(
            "[[flock]]\nname = \"home\"\ndefault = true\n[[flock]]\nname = \"work\"\n\n[[machine]]\nname = \"a\"\nlocal = true\nflock = \"work\"\n",
        )
        .unwrap();
        f.save(&path).unwrap();
        assert_eq!(Flock::load(&path).unwrap(), f);
    }

    fn home_and_work() -> Flock {
        flocks(
            "[[flock]]\nname = \"home\"\ndefault = true\n[[flock]]\nname = \"work\"\n\n[[machine]]\nname = \"h\"\nlocal = true\n\n[[machine]]\nname = \"w\"\nssh = \"w\"\nflock = \"work\"\n",
        )
        .unwrap()
    }

    /// Naming a flock picks it; a pinned machine settles it; neither is the
    /// default flock. A machine outside the flock asked for is refused.
    #[test]
    fn task_flock_follows_the_request_then_the_pin_then_the_default() {
        let f = home_and_work();
        assert_eq!(f.task_flock(None, None).unwrap(), "home");
        assert_eq!(f.task_flock(Some("work"), None).unwrap(), "work");
        assert_eq!(f.task_flock(None, Some("w")).unwrap(), "work");
        assert_eq!(f.task_flock(Some("work"), Some("w")).unwrap(), "work");
        assert_eq!(
            f.task_flock(Some("home"), Some("w")).unwrap_err(),
            TaskFlockError::MachineElsewhere {
                machine: "w".into(),
                flocks: vec!["work".into()],
                requested: "home".into(),
            }
        );
        assert_eq!(
            f.task_flock(Some("play"), None).unwrap_err(),
            TaskFlockError::UnknownFlock("play".into())
        );
        assert_eq!(
            f.task_flock(Some("home"), Some("w"))
                .unwrap_err()
                .to_string(),
            "machine w is in flock work, not home"
        );
        // A pin to a machine the file does not have settles nothing; the
        // caller refuses the machine on its own terms.
        assert_eq!(f.task_flock(None, Some("gone")).unwrap(), "home");
    }

    const MANY: &str = r#"
[[flock]]
name = "home"
default = true
machines = { desk = 3, lab = 1 }

[[flock]]
name = "work"
machines = { desk = 2 }

[[flock]]
name = "play"

[[machine]]
name = "desk"
local = true
max_agents = 4

[[machine]]
name = "lab"
ssh = "user@lab"
flock = "play"

[[machine]]
name = "spare"
ssh = "user@spare"
"#;

    /// A flock's `machines` lists the machines it may use with its number
    /// on each; a machine can be in many. The old `flock` key is membership
    /// with no number but the machine's own limits, and a machine nothing
    /// places is in the default flock the same way.
    #[test]
    fn a_machine_can_be_in_many_flocks_each_with_its_number() {
        let f = flocks(MANY).unwrap();
        assert_eq!(
            f.machine_flocks("desk").unwrap(),
            [
                ("home", Some(FlockNumber::plain(3))),
                ("work", Some(FlockNumber::plain(2)))
            ]
        );
        assert_eq!(
            f.machine_flocks("lab").unwrap(),
            [("home", Some(FlockNumber::plain(1))), ("play", None)]
        );
        assert_eq!(f.machine_flocks("spare").unwrap(), [("home", None)]);
        assert_eq!(f.machine_flocks("gone"), None);
        assert!(f.in_flock(f.get("desk").unwrap(), "work"));
        assert!(!f.in_flock(f.get("desk").unwrap(), "play"));
        assert_eq!(f.members("home"), ["desk", "lab", "spare"]);
        assert_eq!(f.members("work"), ["desk"]);
        assert_eq!(f.members("play"), ["lab"]);
        // One flock stands for the machine where one name is wanted: the
        // default when it is in it, else its first.
        assert_eq!(f.machine_flock("desk"), Some("home"));
        let only_work = flocks(&MANY.replace("desk = 3, ", "")).unwrap();
        assert_eq!(only_work.machine_flock("desk"), Some("work"));
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        f.save(&path).unwrap();
        assert_eq!(Flock::load(&path).unwrap(), f);
    }

    #[test]
    fn membership_errors_fail_the_load() {
        let unknown = MANY.replace("desk = 2", "nope = 2");
        assert_eq!(
            flocks(&unknown).unwrap_err(),
            "flock work: machine nope is not declared"
        );
        let twice = MANY.replace(
            "name = \"play\"\n",
            "name = \"play\"\nmachines = { lab = 1 }\n",
        );
        assert_eq!(
            flocks(&twice).unwrap_err(),
            "machine lab: in flock play twice, by its flock key and by the flock's machines"
        );
        assert!(flocks(&MANY.replace("desk = 2", "desk = \"two\"")).is_err());
    }

    /// 0 keeps a machine in a flock with no slots, to drain it: the plain
    /// number and a share and a max both at 0 load, `flock join --max 0`
    /// writes it, and a negative or fractional number stays refused.
    #[test]
    fn a_number_of_0_drains_the_machine() {
        let f = flocks(&MANY.replace("desk = 2", "desk = 0")).unwrap();
        let work = f.entry("work").unwrap().machines["desk"];
        assert_eq!((work.share(), work.max()), (0, 0));
        assert_eq!(work.to_string(), "0");
        let split = flocks(&MANY.replace("desk = 2", "desk = { share = 0, max = 0 }")).unwrap();
        let work = split.entry("work").unwrap().machines["desk"];
        assert_eq!((work.share(), work.max()), (0, 0));
        for bad in ["desk = -1", "desk = 1.5", "desk = { share = -1, max = 0 }"] {
            assert!(flocks(&MANY.replace("desk = 2", bad)).is_err(), "{bad}");
        }
        assert_eq!(
            flocks(&MANY.replace("desk = 2", "desk = { share = 1, max = 0 }")).unwrap_err(),
            "flock work: machine desk: max 0 is below share 1"
        );
        let mut d = FlockDoc::parse(MANY).unwrap();
        assert_eq!(
            d.join_flock("desk", "work", Some(0)).unwrap(),
            FlockNumber::plain(0)
        );
        assert!(d.to_string().contains("machines = { desk = 0 }"), "{d}");
    }

    /// A flock's number on a machine can be a share and a max. The plain
    /// number is both; `max` below `share`, `max` alone, a share alone and
    /// a share of 0 fail the load. The file saves back as it was written.
    #[test]
    fn a_number_can_be_a_share_and_a_max() {
        let f = flocks(&MANY.replace("desk = 2", "desk = { share = 2, max = 4 }")).unwrap();
        let work = f.entry("work").unwrap().machines["desk"];
        assert_eq!((work.share(), work.max()), (2, 4));
        assert_eq!(work.to_string(), "2/4");
        let home = f.entry("home").unwrap().machines["desk"];
        assert_eq!((home.share(), home.max()), (3, 3));
        assert_eq!(home.to_string(), "3");
        assert_eq!(
            f.machine_flocks("desk").unwrap(),
            [
                ("home", Some(FlockNumber::plain(3))),
                ("work", Some(FlockNumber::split(2, 4)))
            ]
        );
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        f.save(&path).unwrap();
        assert_eq!(Flock::load(&path).unwrap(), f);

        let err = |number: &str| flocks(&MANY.replace("desk = 2", number)).unwrap_err();
        assert_eq!(
            err("desk = { share = 3, max = 2 }"),
            "flock work: machine desk: max 2 is below share 3"
        );
        assert_eq!(
            err("desk = { max = 4 }"),
            "flock work: machine desk: max alone is not allowed; a plain number (`= N`) is the hard ceiling"
        );
        assert_eq!(
            err("desk = { share = 2 }"),
            "flock work: machine desk: a share needs a max; a plain number (`= N`) is the hard ceiling"
        );
        assert_eq!(
            err("desk = { share = 0, max = 2 }"),
            "flock work: machine desk: the share must be at least 1 while max is above 0"
        );
        assert!(flocks(&MANY.replace("desk = 2", "desk = { share = 2, most = 4 }")).is_err());
        // Joining again keeps a share and a max; `--max` makes it plain.
        let mut d =
            FlockDoc::parse(&MANY.replace("desk = 2", "desk = { share = 2, max = 4 }")).unwrap();
        assert_eq!(
            d.join_flock("desk", "work", None).unwrap(),
            FlockNumber::split(2, 4)
        );
        assert!(
            d.to_string().contains("desk = { share = 2, max = 4 }"),
            "{d}"
        );
        assert_eq!(
            d.join_flock("desk", "work", Some(3)).unwrap(),
            FlockNumber::plain(3)
        );
        assert!(d.to_string().contains("machines = { desk = 3 }"), "{d}");
        // Equal share and max load, as the plain number.
        let same = flocks(&MANY.replace("desk = 2", "desk = { share = 2, max = 2 }")).unwrap();
        assert_eq!(
            same.entry("work").unwrap().machines["desk"].to_string(),
            "2"
        );
    }

    /// A pinned machine in many flocks: a named flock must be one of them,
    /// and none named is the one that stands for the machine.
    #[test]
    fn task_flock_with_a_machine_in_many_flocks() {
        let f = flocks(MANY).unwrap();
        assert_eq!(f.task_flock(Some("work"), Some("desk")).unwrap(), "work");
        assert_eq!(f.task_flock(None, Some("desk")).unwrap(), "home");
        assert_eq!(
            f.task_flock(Some("play"), Some("desk"))
                .unwrap_err()
                .to_string(),
            "machine desk is in flocks home, work, not play"
        );
    }

    /// Removing a machine takes it out of every flock's `machines`, and a
    /// flock that lists a machine has it for `flock remove`.
    #[test]
    fn machine_remove_leaves_the_flocks_and_listed_machines_hold_a_flock() {
        let mut d = FlockDoc::parse(MANY).unwrap();
        assert_eq!(
            d.remove_flock("work", &[]).unwrap_err(),
            EditError::FlockHasMachines {
                flock: "work".into(),
                machines: vec!["desk".into()],
            }
        );
        d.remove_machine("desk").unwrap();
        let f = d.flock().unwrap();
        assert!(f.entry("work").unwrap().machines.is_empty());
        assert_eq!(f.members("home"), ["lab", "spare"]);
        assert!(d.to_string().contains("machines = { lab = 1 }"), "{d}");
        d.remove_flock("work", &[]).unwrap();
    }

    #[test]
    fn a_flock_entry_can_carry_an_agent_and_its_args() {
        let f = flocks(
            "[[flock]]\nname = \"home\"\ndefault = true\n[[flock]]\nname = \"work\"\nagent = \"codex\"\nagent_args = [\"--model\", \"gpt-x\"]\n",
        )
        .unwrap();
        let work = f.entry("work").unwrap();
        assert_eq!(work.agent.as_deref(), Some("codex"));
        assert_eq!(
            work.agent_args.as_deref(),
            Some(&["--model".to_string(), "gpt-x".to_string()][..])
        );
        let home = f.entry("home").unwrap();
        assert_eq!(
            (home.agent.as_ref(), home.agent_args.as_ref()),
            (None, None)
        );
        assert!(f.entry("play").is_none());
        // The implicit flock of a file with no `[[flock]]` has no entry.
        assert!(Flock::default().entry(DEFAULT_FLOCK).is_none());
    }

    #[test]
    fn a_flock_entry_can_carry_tool_lists_and_bad_patterns_fail_the_load() {
        let f = flocks(
            "[[flock]]\nname = \"home\"\ndefault = true\nallow = [\"Edit\"]\ndeny = [\"Bash(rm:*)\"]\n",
        )
        .unwrap();
        assert_eq!(f.entry("home").unwrap().allow, vec!["Edit"]);
        assert_eq!(f.entry("home").unwrap().deny, vec!["Bash(rm:*)"]);
        let err =
            flocks("[[flock]]\nname = \"home\"\ndefault = true\ndeny = [\"-x\"]\n").unwrap_err();
        assert!(err.contains("flock home: deny"), "{err}");
    }

    /// `machine add` refuses a name `authkeys::valid_name` refuses, naming
    /// the rule, before anything is written; a good one is added.
    #[test]
    fn adding_a_machine_follows_the_naming_rule() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        for bad in [
            "", "-pi", "--agent", ".pi", "_pi", "pi 4", "pi;id", "pi$(id)", "pi|x", "pi\nx",
            "pi/4", "pi'4", "é1",
        ] {
            let err = d.add_machine(&pi(bad)).unwrap_err();
            assert_eq!(err, EditError::InvalidMachineName(bad.into()), "{bad:?}");
            assert_eq!(err.code(), "invalid_machine");
            assert!(err.to_string().contains(MACHINE_NAME_RULE), "{err}");
        }
        assert_eq!(d.to_string(), COMMENTED, "nothing was written");
        for ok in ["pi-4", "Build_01", "a.b", "9", "x"] {
            d.add_machine(&pi(ok)).unwrap();
        }
    }

    /// A flock written before the rule still loads; the machine is named, with
    /// the rule, so it can be renamed. A good flock says nothing.
    #[test]
    fn an_old_flock_with_an_odd_name_loads_and_says_so() {
        let text = "[[machine]]\nname = \"-old\"\nssh = \"u@a\"\n[[machine]]\nname = \"fine-1\"\nssh = \"u@b\"\n[[machine]]\nname = \"two words\"\nssh = \"u@c\"\n";
        let f = Flock::parse(std::path::Path::new("flock.toml"), text).unwrap();
        let w = f.machine_name_warnings();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(
            w[0].contains("\"-old\"") && w[0].contains(MACHINE_NAME_RULE),
            "{w:?}"
        );
        assert!(w[1].contains("two words"), "{w:?}");
        assert!(!w.iter().any(|l| l.contains("fine-1")));
        let good = Flock::parse(
            std::path::Path::new("flock.toml"),
            "[[machine]]\nname = \"pi-1\"\nlocal = true\n",
        )
        .unwrap();
        assert!(good.machine_name_warnings().is_empty());
    }

    /// The load warning is said once for an unchanged file however often it
    /// is read, and again when the file changes, as `pastor serve` reads it.
    #[test]
    fn the_warning_is_said_again_when_the_file_changes() {
        use std::io::Write;
        #[derive(Clone, Default)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
            type Writer = Capture;
            fn make_writer(&'a self) -> Capture {
                self.clone()
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let said = || {
            String::from_utf8(capture.0.lock().unwrap().clone())
                .unwrap()
                .matches("breaks the naming rule")
                .count()
        };
        let one = "[[machine]]\nname = \"-old-name\"\nssh = \"u@a\"\n";
        let two = "[[machine]]\nname = \"-old-name\"\nssh = \"u@a\"\n[[machine]]\nname = \"pi-2\"\nssh = \"u@b\"\n";
        tracing::subscriber::with_default(subscriber, || {
            std::fs::write(&path, one).unwrap();
            Flock::load(&path).unwrap();
            Flock::load_existing(&path).unwrap();
            assert_eq!(said(), 1, "an unchanged file is said once");
            std::fs::write(&path, two).unwrap();
            Flock::load_existing(&path).unwrap();
            assert_eq!(said(), 2, "a changed file is said again");
            Flock::load_existing(&path).unwrap();
            Flock::load(&path).unwrap();
            assert_eq!(said(), 2, "and once for that version");
            // Back to the first text: a version already said stays said.
            std::fs::write(&path, one).unwrap();
            Flock::load_existing(&path).unwrap();
            assert_eq!(said(), 2);
        });
    }

    const COMMENTED: &str = "# my fleet\n\n[[machine]]\nname = \"pi-1\"   # the desk one\nlocal = true\n\n# spare\n[[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n";

    #[test]
    fn edits_keep_comments_and_layout() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_machine(&pi("pi-4")).unwrap();
        let text = d.to_string();
        assert!(text.starts_with(COMMENTED), "{text}");
        assert!(
            text.contains("\n\n[[machine]]\nname = \"pi-4\"\n"),
            "{text}"
        );
        d.remove_machine("pi-4").unwrap();
        assert_eq!(d.to_string(), COMMENTED);
        assert_eq!(
            d.remove_machine("pi-9").unwrap_err(),
            EditError::UnknownMachine("pi-9".into())
        );
        assert_eq!(
            d.add_machine(&pi("pi-3")).unwrap_err(),
            EditError::MachineExists("pi-3".into())
        );
    }

    /// A flock and a machine may say what they are for; a file without
    /// descriptions loads as before. `flock add` and `machine add` write
    /// one when given.
    #[test]
    fn flocks_and_machines_take_a_description() {
        let f: Flock = toml::from_str(
            "[[flock]]\nname = \"life\"\ndefault = true\ndescription = \"Personal errands\"\n\n\
             [[machine]]\nname = \"pi-1\"\nssh = \"user@pi-1\"\ndescription = \"The desk one\"\n\n\
             [[machine]]\nname = \"pi-2\"\nssh = \"user@pi-2\"\n",
        )
        .unwrap();
        assert_eq!(
            f.entry("life").unwrap().description.as_deref(),
            Some("Personal errands")
        );
        assert_eq!(
            f.get("pi-1").unwrap().description.as_deref(),
            Some("The desk one")
        );
        assert_eq!(f.get("pi-2").unwrap().description, None);
        let old: Flock = toml::from_str(COMMENTED).unwrap();
        assert!(old.machines.iter().all(|m| m.description.is_none()));

        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_flock("work", false, &[]).unwrap();
        d.describe_flock("work", "Paid work").unwrap();
        d.add_machine(&MachineConfig {
            description: Some("A spare".into()),
            ..pi("pi-9")
        })
        .unwrap();
        let f = d.flock().unwrap();
        assert_eq!(
            f.entry("work").unwrap().description.as_deref(),
            Some("Paid work")
        );
        assert_eq!(
            f.get("pi-9").unwrap().description.as_deref(),
            Some("A spare")
        );
        assert!(d.describe_flock("nope", "x").is_err());
    }

    /// The first named flock puts the implicit one on paper, as the default,
    /// at the head of the file; the machines stay where they were.
    #[test]
    fn adding_a_flock_declares_the_implicit_default_first() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_flock("work", false, &[]).unwrap();
        let text = d.to_string();
        assert!(
            text.starts_with("# my fleet\n\n[[flock]]\nname = \"default\"\ndefault = true\n\n[[flock]]\nname = \"work\"\n\n[[machine]]\nname = \"pi-1\"   # the desk one\n"),
            "{text}"
        );
        let f = d.flock().unwrap();
        assert_eq!(f.flock_names(), ["default", "work"]);
        assert_eq!(f.machine_flock("pi-1"), Some("default"));
        d.add_flock("play", false, &[]).unwrap();
        assert_eq!(
            d.flock().unwrap().flock_names(),
            ["default", "work", "play"]
        );
        assert!(
            d.to_string()
                .contains("name = \"work\"\n\n[[flock]]\nname = \"play\"\n\n[[machine]]"),
            "{d}"
        );
        assert_eq!(
            d.add_flock("work", false, &[]).unwrap_err(),
            EditError::FlockExists("work".into())
        );
    }

    /// Without `--default` the first named flock leaves the machines in the
    /// implicit one, and says so.
    #[test]
    fn a_first_flock_reports_the_machines_stay_in_default() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.add_flock("work", false, &[]).unwrap(),
            FlockAdded {
                flock: "default".into(),
                machines: vec!["pi-1".into(), "pi-3".into()],
                moved: false,
                held_by: Vec::new(),
            }
        );
        assert_eq!(d.flock().unwrap().machine_flock("pi-1"), Some("default"));
    }

    /// The first named flock added as the default replaces the implicit one:
    /// the machines that name no flock follow it, and no `default` flock is
    /// written down to sit empty.
    #[test]
    fn a_first_default_flock_takes_the_machines_along() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.add_flock("personal", true, &[]).unwrap(),
            FlockAdded {
                flock: "personal".into(),
                machines: vec!["pi-1".into(), "pi-3".into()],
                moved: true,
                held_by: Vec::new(),
            }
        );
        let f = d.flock().unwrap();
        assert_eq!(f.flock_names(), ["personal"]);
        assert_eq!(f.default_flock(), "personal");
        assert_eq!(f.machine_flock("pi-1"), Some("personal"));
        assert_eq!(f.machine_flock("pi-3"), Some("personal"));
        let text = d.to_string();
        assert!(
            text.starts_with("# my fleet\n\n[[flock]]\nname = \"personal\"\ndefault = true\n\n[[machine]]\nname = \"pi-1\"   # the desk one\n"),
            "{text}"
        );
        assert!(!text.contains("flock = "), "{text}");

        // A machine that names the implicit flock stays in it, so that one
        // is declared, just not as the default.
        let named = format!(
            "{COMMENTED}\n[[machine]]\nname = \"pi-5\"\nlocal = true\nflock = \"default\"\n"
        );
        let mut d = FlockDoc::parse(&named).unwrap();
        let added = d.add_flock("personal", true, &[]).unwrap();
        assert_eq!(added.machines, ["pi-1", "pi-3"]);
        assert!(added.moved);
        let f = d.flock().unwrap();
        assert_eq!(f.flock_names(), ["default", "personal"]);
        assert_eq!(f.default_flock(), "personal");
        assert_eq!(f.machine_flock("pi-1"), Some("personal"));
        assert_eq!(f.machine_flock("pi-5"), Some("default"));

        // With no machines there is nothing to move.
        let mut d = FlockDoc::parse("").unwrap();
        assert_eq!(
            d.add_flock("personal", true, &[]).unwrap().machines,
            Vec::<String>::new()
        );
        assert_eq!(d.flock().unwrap().flock_names(), ["personal"]);

        // Queued tasks in the implicit flock keep the machines there, so the
        // tasks still have somewhere to run.
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.add_flock("personal", true, &["t-4".into()]).unwrap(),
            FlockAdded {
                flock: "default".into(),
                machines: vec!["pi-1".into(), "pi-3".into()],
                moved: false,
                held_by: vec!["t-4".into()],
            }
        );
        let f = d.flock().unwrap();
        assert_eq!(f.flock_names(), ["default", "personal"]);
        assert_eq!(f.default_flock(), "personal");
        assert_eq!(f.machine_flock("pi-1"), Some("default"));
    }

    #[test]
    fn moving_a_machine_names_its_flock() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.move_machine("pi-3", "work").unwrap_err(),
            EditError::UnknownFlock("work".into())
        );
        d.add_flock("work", false, &[]).unwrap();
        d.move_machine("pi-3", "work").unwrap();
        assert_eq!(d.flock().unwrap().machine_flock("pi-3"), Some("work"));
        assert!(d.to_string().contains("# spare\n"), "{d}");
        assert_eq!(
            d.move_machine("pi-9", "work").unwrap_err(),
            EditError::UnknownMachine("pi-9".into())
        );
        let mut m = pi("pi-5");
        m.flock = Some("nope".into());
        assert_eq!(
            d.add_machine(&m).unwrap_err(),
            EditError::UnknownFlock("nope".into())
        );
        m.flock = Some("work".into());
        d.add_machine(&m).unwrap();
        assert_eq!(d.flock().unwrap().machine_flock("pi-5"), Some("work"));
    }

    /// Moving a machine into a flock whose `machines` table already lists it
    /// is a no-op: writing the `flock` key too would place it in the same
    /// flock twice, by both means, which `validate` refuses.
    #[test]
    fn moving_a_machine_into_a_flock_that_already_lists_it_is_a_no_op() {
        let text = format!(
            "{COMMENTED}\n[[flock]]\nname = \"default\"\ndefault = true\n\n[[flock]]\nname = \"work\"\nmachines = {{ pi-3 = 2 }}\n"
        );
        let mut d = FlockDoc::parse(&text).unwrap();
        d.move_machine("pi-3", "work").unwrap();
        assert_eq!(d.to_string(), text);
        assert_eq!(d.flock().unwrap().machine_flock("pi-3"), Some("work"));
    }

    /// `flock join` lists the machine in the flock's `machines`, by default
    /// with its `max_agents`; joining again with a number changes it, and
    /// without one keeps it.
    #[test]
    fn joining_a_flock_lists_the_machine_with_its_number() {
        let mut d = FlockDoc::parse(MANY).unwrap();
        d.join_flock("desk", "play", None).unwrap();
        let f = d.flock().unwrap();
        assert_eq!(
            f.entry("play").unwrap().machines["desk"],
            FlockNumber::plain(4)
        );
        assert_eq!(
            f.machine_flocks("desk").unwrap(),
            [
                ("home", Some(FlockNumber::plain(3))),
                ("work", Some(FlockNumber::plain(2))),
                ("play", Some(FlockNumber::plain(4)))
            ]
        );
        d.join_flock("desk", "work", Some(1)).unwrap();
        assert_eq!(
            d.flock().unwrap().entry("work").unwrap().machines["desk"],
            FlockNumber::plain(1)
        );
        d.join_flock("desk", "work", None).unwrap();
        assert_eq!(
            d.flock().unwrap().entry("work").unwrap().machines["desk"],
            FlockNumber::plain(1)
        );
        assert!(d.to_string().contains("machines = { desk = 1 }"), "{d}");
        // A machine nothing placed leaves the default once a flock lists it.
        d.join_flock("spare", "work", Some(2)).unwrap();
        assert_eq!(
            d.flock().unwrap().machine_flocks("spare").unwrap(),
            [("work", Some(FlockNumber::plain(2)))]
        );
        assert_eq!(
            d.join_flock("nope", "work", None).unwrap_err(),
            EditError::UnknownMachine("nope".into())
        );
        assert_eq!(
            d.join_flock("desk", "nope", None).unwrap_err(),
            EditError::UnknownFlock("nope".into())
        );
    }

    /// The first membership edit of a machine with the old `flock` key moves
    /// the key into that flock's `machines`, with the machine's
    /// `max_agents`, and keeps the comments around it.
    #[test]
    fn a_membership_edit_moves_the_old_flock_key_into_the_table() {
        let text = format!(
            "{COMMENTED}flock = \"work\"   # for now\n\n# the work flock\n[[flock]]\nname = \"default\"\ndefault = true\n\n[[flock]]\nname = \"work\"\n"
        );
        let mut d = FlockDoc::parse(&text).unwrap();
        d.join_flock("pi-3", "default", Some(1)).unwrap();
        let out = d.to_string();
        assert!(!out.contains("flock = \"work\""), "{out}");
        assert!(out.contains("# the work flock\n"), "{out}");
        assert!(
            out.contains("# spare\n") && out.contains("# my fleet\n"),
            "{out}"
        );
        assert!(out.contains("machines = { pi-3 = 2 }"), "{out}");
        assert_eq!(
            d.flock().unwrap().machine_flocks("pi-3").unwrap(),
            [
                ("default", Some(FlockNumber::plain(1))),
                ("work", Some(FlockNumber::plain(2)))
            ]
        );

        // Joining the flock the key names takes the number given.
        let mut d = FlockDoc::parse(&text).unwrap();
        d.join_flock("pi-3", "work", Some(3)).unwrap();
        assert_eq!(
            d.flock().unwrap().machine_flocks("pi-3").unwrap(),
            [("work", Some(FlockNumber::plain(3)))]
        );

        // So does moving it to the flock the key already names.
        let mut d = FlockDoc::parse(&text).unwrap();
        d.move_machine("pi-3", "work").unwrap();
        let out = d.to_string();
        assert!(!out.contains("flock = \"work\""), "{out}");
        assert!(out.contains("machines = { pi-3 = 2 }"), "{out}");
        assert_eq!(
            d.flock().unwrap().machine_flocks("pi-3").unwrap(),
            [("work", Some(FlockNumber::plain(2)))]
        );

        // A file with no `[[flock]]` gets its implicit flock declared.
        let old = format!("{COMMENTED}flock = \"default\"\n");
        let mut d = FlockDoc::parse(&old).unwrap();
        d.leave_flock("pi-3", "default").unwrap();
        let f = d.flock().unwrap();
        assert_eq!(f.default_flock(), "default");
        assert!(f.unplaced(f.get("pi-3").unwrap()), "{d}");
    }

    /// `flock leave` takes the machine out; out of its last flock it is back
    /// in the default flock, as a machine no flock lists.
    #[test]
    fn leaving_a_flock_and_the_last_one() {
        let mut d = FlockDoc::parse(MANY).unwrap();
        d.leave_flock("desk", "work").unwrap();
        let f = d.flock().unwrap();
        assert_eq!(
            f.machine_flocks("desk").unwrap(),
            [("home", Some(FlockNumber::plain(3)))]
        );
        assert!(!d.to_string().contains("machines = {}"), "{d}");
        assert!(f.entry("work").unwrap().machines.is_empty());
        // lab is in home by the table and in play by its old key, which
        // the edit moves into play's table.
        d.leave_flock("lab", "home").unwrap();
        assert_eq!(
            d.flock().unwrap().machine_flocks("lab").unwrap(),
            [("play", Some(FlockNumber::plain(2)))]
        );
        d.leave_flock("lab", "play").unwrap();
        let f = d.flock().unwrap();
        assert!(f.unplaced(f.get("lab").unwrap()));
        assert_eq!(f.machine_flocks("lab").unwrap(), [("home", None)]);
        assert_eq!(
            d.leave_flock("desk", "play").unwrap_err(),
            EditError::NotInFlock {
                machine: "desk".into(),
                flock: "play".into()
            }
        );
        // In the default only because nothing lists it: nothing to leave.
        assert_eq!(
            d.leave_flock("spare", "home").unwrap_err().code(),
            "not_in_flock"
        );
        assert_eq!(
            d.leave_flock("desk", "nope").unwrap_err(),
            EditError::UnknownFlock("nope".into())
        );
    }

    /// `machine move` leaves every flock and joins the one named with the
    /// machine's `max_agents`.
    #[test]
    fn moving_a_machine_leaves_every_flock_for_one() {
        let mut d = FlockDoc::parse(MANY).unwrap();
        d.move_machine("desk", "play").unwrap();
        d.move_machine("lab", "work").unwrap();
        let f = d.flock().unwrap();
        assert_eq!(
            f.machine_flocks("desk").unwrap(),
            [("play", Some(FlockNumber::plain(4)))]
        );
        assert_eq!(
            f.machine_flocks("lab").unwrap(),
            [("work", Some(FlockNumber::plain(2)))]
        );
        assert!(f.get("lab").unwrap().flock.is_none());
        assert!(f.entry("home").unwrap().machines.is_empty());
    }

    /// Changing the default changes where new work goes, not where the
    /// machines are: those with no flock of their own get the old one.
    #[test]
    fn a_new_default_keeps_the_machines_in_their_flocks() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_flock("play", false, &[]).unwrap();
        assert_eq!(
            d.add_flock("work", true, &[]).unwrap(),
            FlockAdded {
                flock: "default".into(),
                machines: vec!["pi-1".into(), "pi-3".into()],
                moved: false,
                held_by: Vec::new(),
            }
        );
        let f = d.flock().unwrap();
        assert_eq!(f.default_flock(), "work");
        assert_eq!(f.machine_flock("pi-1"), Some("default"));
        assert_eq!(f.machine_flock("pi-3"), Some("default"));
        d.set_default("default").unwrap();
        assert_eq!(d.flock().unwrap().default_flock(), "default");
        d.set_default("default").unwrap();
        assert_eq!(
            d.set_default("nope").unwrap_err(),
            EditError::UnknownFlock("nope".into())
        );
    }

    #[test]
    fn a_flock_is_removed_only_when_empty_idle_and_not_the_default() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_flock("work", false, &[]).unwrap();
        d.move_machine("pi-3", "work").unwrap();
        assert_eq!(
            d.remove_flock("work", &[]).unwrap_err(),
            EditError::FlockHasMachines {
                flock: "work".into(),
                machines: vec!["pi-3".into()],
            }
        );
        assert_eq!(
            d.remove_flock("default", &[]).unwrap_err(),
            EditError::RemovingDefault("default".into())
        );
        assert_eq!(
            d.remove_flock("nope", &[]).unwrap_err(),
            EditError::UnknownFlock("nope".into())
        );
        d.move_machine("pi-3", "default").unwrap();
        assert_eq!(
            d.remove_flock("work", &["t-7".into()]).unwrap_err(),
            EditError::FlockHasTasks {
                flock: "work".into(),
                tasks: vec!["t-7".into()],
            }
        );
        d.remove_flock("work", &[]).unwrap();
        assert_eq!(d.flock().unwrap().flock_names(), ["default"]);
    }

    #[test]
    fn the_implicit_default_flock_is_refused_as_the_default() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.remove_flock("default", &[]).unwrap_err(),
            EditError::RemovingDefault("default".into())
        );
    }

    #[test]
    fn a_doc_saves_atomically_and_refuses_a_bad_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let mut d = FlockDoc::open(&path).unwrap();
        d.add_machine(&pi("a")).unwrap();
        d.save(&path).unwrap();
        assert_eq!(Flock::load(&path).unwrap().machines, vec![pi("a")]);
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::write(&path, "[[flock]]\nname = \"x\"\n").unwrap();
        let err = format!("{:#}", FlockDoc::open(&path).err().unwrap());
        assert!(err.contains("no flock has default"), "{err}");
    }

    #[test]
    fn an_empty_command_is_refused() {
        let f = Flock {
            flocks: vec![],
            machines: vec![MachineConfig {
                ssh: None,
                command: Some(vec![]),
                ..pi("x")
            }],
        };
        assert_eq!(f.validate().unwrap_err(), "machine x: command is empty");
    }

    /// ssh reads a target starting with `-` as an option, and
    /// `-oProxyCommand=...` runs a local command at the next connect. A
    /// target is one word: no option, no whitespace, no control characters.
    #[test]
    fn an_ssh_target_that_is_not_one_plain_word_is_refused() {
        for (target, why) in [
            ("-oProxyCommand=sh -c x", "starts with '-'"),
            ("-p2222", "starts with '-'"),
            ("", "is empty"),
            ("user@pi 3", "whitespace"),
            ("pi-3\n", "control"),
        ] {
            let f = Flock {
                flocks: vec![],
                machines: vec![MachineConfig {
                    ssh: Some(target.into()),
                    ..pi("x")
                }],
            };
            let err = f.validate().unwrap_err();
            assert!(
                err.starts_with("machine x: ssh") && err.contains(why),
                "{target:?}: {err}"
            );
        }
        for target in ["pi-3", "fleet@pi-3", "fleet@192.0.2.3", "pi-3.lan"] {
            let f = Flock {
                flocks: vec![],
                machines: vec![MachineConfig {
                    ssh: Some(target.into()),
                    ..pi("x")
                }],
            };
            f.validate().unwrap_or_else(|e| panic!("{target}: {e}"));
        }
    }

    /// A session name goes into a command a remote login shell parses, and
    /// fish reads a backslash inside single quotes as an escape, so one could
    /// end the quoted word early there.
    #[test]
    fn a_session_name_with_a_backslash_or_control_character_is_refused() {
        for session in ["a\\b", "x'\\", "s\n"] {
            let f = Flock {
                flocks: vec![],
                machines: vec![MachineConfig {
                    session: session.into(),
                    ..pi("x")
                }],
            };
            let err = f.validate().unwrap_err();
            assert!(err.starts_with("machine x: session"), "{session:?}: {err}");
        }
    }

    /// A machine takes its own loads and those of every flock it is in,
    /// each once; a machine outside the flock that lists a load does not
    /// take it.
    #[test]
    fn a_machine_takes_its_own_loads_and_its_flocks() {
        let text = r#"
[[flock]]
name = "default"
default = true
machines = { a = 2, b = 2, c = 2 }

[[flock]]
name = "rust"
machines = { b = 1 }
loads = ["rust"]

[[machine]]
name = "a"
local = true

[[machine]]
name = "b"
ssh = "b"
loads = ["light", "rust"]

[[machine]]
name = "c"
ssh = "c"
"#;
        let f = Flock::parse(Path::new("flock.toml"), text).unwrap();
        let loads = |name: &str| f.loads_of(f.get(name).unwrap());
        assert!(loads("a").is_empty());
        assert_eq!(
            loads("b"),
            ["light", "rust"],
            "its own, then its flocks', once"
        );
        assert!(loads("c").is_empty());
        assert_eq!(
            f.takers("default", "rust"),
            ["b"],
            "a flock's load counts in any flock"
        );
        assert_eq!(f.takers("rust", "rust"), ["b"]);
        assert!(f.takers("default", "gpu").is_empty());
    }

    /// A load in flock.toml is a load name, listed once.
    #[test]
    fn loads_are_checked() {
        let machine = |loads: &[&str]| Flock {
            flocks: vec![],
            machines: vec![MachineConfig {
                loads: loads.iter().map(|l| l.to_string()).collect(),
                ..pi("x")
            }],
        };
        machine(&["rust", "light_2"]).validate().unwrap();
        let err = machine(&["rust", "rust"]).validate().unwrap_err();
        assert!(
            err.contains("machine x: loads: rust is listed twice"),
            "{err}"
        );
        let err = machine(&["no space"]).validate().unwrap_err();
        assert!(err.contains("machine x: loads:"), "{err}");
        let flock = Flock {
            flocks: vec![FlockEntry {
                name: "f".into(),
                default: true,
                loads: vec!["-rust".into()],
                ..Default::default()
            }],
            machines: vec![],
        };
        let err = flock.validate().unwrap_err();
        assert!(err.contains("flock f: loads:"), "{err}");
    }

    #[test]
    fn parses_spec_example() {
        let text = r#"
[[machine]]
name = "pi-1"
local = true
max_agents = 2

[[machine]]
name = "pi-3"
ssh = "fleet@pi-3"
session = "default"
max_agents = 3
tags = ["fast"]
"#;
        let f: Flock = toml::from_str(text).unwrap();
        assert_eq!(f.machines.len(), 2);
        assert!(f.machines[0].local);
        assert_eq!(f.machines[1].ssh.as_deref(), Some("fleet@pi-3"));
        assert_eq!(f.machines[1].tags, vec!["fast"]);
        f.validate().unwrap();
    }

    #[test]
    fn missing_file_is_empty_flock() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Flock::load(&tmp.path().join("flock.toml")).unwrap();
        assert!(f.machines.is_empty());
    }

    /// `load_existing` is for a reload, where a missing file must not be
    /// mistaken for an intentionally emptied flock (Copilot 4103271200).
    #[test]
    fn load_existing_errors_on_a_missing_path_and_reads_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let err = Flock::load_existing(&path).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound)
        );

        std::fs::write(&path, "").unwrap();
        let f = Flock::load_existing(&path).unwrap();
        assert!(f.machines.is_empty());
    }

    /// A flock's `remove_worktree` is a yes or no; a flock without it
    /// leaves it to `[defaults]`.
    #[test]
    fn a_flock_remove_worktree_is_a_bool() {
        let flock = |extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!(
                    "[[flock]]\nname = \"p\"\ndefault = true\n{extra}\n[[machine]]\nname = \"m\"\nlocal = true\nflock = \"p\"\n"
                ),
            )
        };
        let remove = |extra: &str| flock(extra).unwrap().entry("p").unwrap().remove_worktree;
        assert_eq!(remove(""), None);
        assert_eq!(remove("remove_worktree = true"), Some(true));
        assert!(flock("remove_worktree = \"yes\"").is_err());
    }

    /// A flock's `keep_pane` is a yes or no; a flock without it leaves it
    /// to `[defaults]`, and a misspelt key is a load error.
    #[test]
    fn a_flock_keep_pane_is_a_bool() {
        let flock = |extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!(
                    "[[flock]]\nname = \"p\"\ndefault = true\n{extra}\n[[machine]]\nname = \"m\"\nlocal = true\nflock = \"p\"\n"
                ),
            )
        };
        assert_eq!(flock("").unwrap().entry("p").unwrap().keep_pane, None);
        assert_eq!(
            flock("keep_pane = true")
                .unwrap()
                .entry("p")
                .unwrap()
                .keep_pane,
            Some(true)
        );
        assert!(flock("keep_pane = \"yes\"").is_err());
        assert!(flock("keep_panes = true").is_err());
    }

    /// A flock's `summary` is one of its words; a flock without it leaves
    /// it to `[defaults]`.
    #[test]
    fn a_flocks_summary_is_one_of_its_words() {
        let flock = |extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!(
                    "[[flock]]\nname = \"p\"\ndefault = true\n{extra}\n[[machine]]\nname = \"m\"\nlocal = true\nflock = \"p\"\n"
                ),
            )
        };
        assert_eq!(flock("").unwrap().entry("p").unwrap().summary, None);
        assert_eq!(
            flock("summary = \"off\"")
                .unwrap()
                .entry("p")
                .unwrap()
                .summary,
            Some(crate::task::SummaryMode::Off)
        );
        assert!(flock("summary = \"sometimes\"").is_err());
    }

    /// A flock's or a machine's `model` must be a model name, and one that
    /// pastor.toml's `[models]` defines.
    #[test]
    fn a_model_must_be_a_name_that_models_define() {
        let flock = |extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!(
                    "[[flock]]\nname = \"p\"\ndefault = true\n{extra}\n[[machine]]\nname = \"m\"\nlocal = true\nflock = \"p\"\n"
                ),
            )
        };
        let err = format!("{:#}", flock("model = \"--model x\"").unwrap_err());
        assert!(err.contains("flock p: model name"), "{err}");
        let f = flock("model = \"sonnet\"").unwrap();
        let models: crate::config::Models =
            toml::from_str("[sonnet]\nkind = \"claude\"\nargs = []\n").unwrap();
        f.check_config(
            &models,
            &Default::default(),
            &Default::default(),
            &crate::provision::Catalog::builtin(),
        )
        .unwrap();
        let err = f
            .check_config(
                &Default::default(),
                &Default::default(),
                &Default::default(),
                &crate::provision::Catalog::builtin(),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("flock p: model sonnet is not in [models]"),
            "{err}"
        );
        let err = format!(
            "{:#}",
            Flock::parse(
                Path::new("flock.toml"),
                "[[machine]]\nname = \"m\"\nlocal = true\nmodel = \"A\"\n"
            )
            .unwrap_err()
        );
        assert!(err.contains("machine m: model name"), "{err}");
    }

    /// Each name in a flock's or a machine's `fallback` must be a model
    /// name, and one that pastor.toml's `[models]` defines.
    #[test]
    fn a_fallback_must_name_models_that_models_define() {
        let parse = |text: &str| Flock::parse(Path::new("flock.toml"), text);
        let err = format!(
            "{:#}",
            parse("[[flock]]\nname = \"p\"\ndefault = true\nfallback = [\"--model x\"]\n")
                .unwrap_err()
        );
        assert!(err.contains("flock p: fallback: model name"), "{err}");
        let err = format!(
            "{:#}",
            parse("[[machine]]\nname = \"m\"\nlocal = true\nfallback = [\"A\"]\n").unwrap_err()
        );
        assert!(err.contains("machine m: fallback: model name"), "{err}");
        let models: crate::config::Models =
            toml::from_str("[sonnet]\nkind = \"claude\"\nargs = []\n").unwrap();
        for (text, says) in [
            (
                "[[flock]]\nname = \"p\"\ndefault = true\nfallback = [\"sonnet\", \"gpt\"]\n",
                "flock.toml: flock p: fallback: model gpt is not in [models]",
            ),
            (
                "[[machine]]\nname = \"m\"\nlocal = true\nfallback = [\"gpt\"]\n",
                "flock.toml: machine m: fallback: model gpt is not in [models]",
            ),
        ] {
            let err = parse(text)
                .unwrap()
                .check_config(
                    &models,
                    &Default::default(),
                    &Default::default(),
                    &crate::provision::Catalog::builtin(),
                )
                .unwrap_err()
                .to_string();
            assert!(err.contains(says), "{err}");
        }
        let f = parse("[[machine]]\nname = \"m\"\nlocal = true\nfallback = []\n").unwrap();
        assert_eq!(f.machines[0].fallback, Some(vec![]));
        f.check_config(
            &models,
            &Default::default(),
            &Default::default(),
            &crate::provision::Catalog::builtin(),
        )
        .unwrap();
    }

    /// A flock's `label` is a label template, checked on load.
    #[test]
    fn a_flock_label_must_be_a_label_template() {
        let flock = |extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!("[[flock]]\nname = \"p\"\ndefault = true\n{extra}\n"),
            )
        };
        let f = flock("label = \"p/{{ task.id }}\"").unwrap();
        assert_eq!(f.flocks[0].label.as_deref(), Some("p/{{ task.id }}"));
        let err = format!("{:#}", flock("label = \"{{ item.title }}\"").unwrap_err());
        assert!(err.contains("flock p: label: unknown placeholder"), "{err}");
        let err = format!("{:#}", flock("label = \" \"").unwrap_err());
        assert!(err.contains("flock p: label must not be empty"), "{err}");
    }

    /// A flock's or a machine's `profile` must be a profile name, and one
    /// that is built in or in pastor.toml's `[profiles]`.
    #[test]
    fn a_profile_must_be_built_in_or_in_profiles() {
        let flock = |extra: &str, machine: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!(
                    "[[flock]]\nname = \"p\"\ndefault = true\n{extra}\n[[machine]]\nname = \"m\"\nlocal = true\nflock = \"p\"\n{machine}\n"
                ),
            )
        };
        let err = format!("{:#}", flock("profile = \"--yolo\"", "").unwrap_err());
        assert!(err.contains("flock p: profile name"), "{err}");
        let err = format!("{:#}", flock("", "profile = \"A\"").unwrap_err());
        assert!(err.contains("machine m: profile name"), "{err}");
        let none = crate::config::Models::default();
        let f = flock("profile = \"develop\"", "profile = \"ci\"").unwrap();
        assert_eq!(f.flocks[0].profile.as_deref(), Some("develop"));
        let err = f
            .check_config(
                &none,
                &Default::default(),
                &Default::default(),
                &crate::provision::Catalog::builtin(),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("machine m: profile ci is not built in"),
            "{err}"
        );
        let profiles: crate::config::profile::Profiles =
            toml::from_str("[ci]\nextends = \"develop\"\n").unwrap();
        f.check_config(
            &none,
            &Default::default(),
            &profiles,
            &crate::provision::Catalog::builtin(),
        )
        .unwrap();
    }

    /// `provisioner` on a machine names a provisioner the catalog has, built
    /// in or copied; the id's alphabet is checked by `validate`, its
    /// existence by `check_config`, since the catalog lives in the data dir.
    #[test]
    fn a_machine_provisioner_must_be_in_the_catalog() {
        use crate::provision::Catalog;
        let flock = |machine: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!("[[machine]]\nname = \"m\"\nlocal = true\n{machine}\n"),
            )
        };
        let err = format!("{:#}", flock("provisioner = \"Mac OS\"").unwrap_err());
        assert!(
            err.contains("machine m: provisioner id \"Mac OS\" must match"),
            "{err}"
        );
        let f = flock("provisioner = \"macos\"").unwrap();
        assert_eq!(f.machines[0].provisioner.as_deref(), Some("macos"));
        let check = |f: &Flock, catalog: &Catalog| {
            f.check_config(
                &Default::default(),
                &Default::default(),
                &Default::default(),
                catalog,
            )
            .map_err(|e| e.to_string())
        };
        check(&f, &Catalog::builtin()).unwrap();
        let f = flock("provisioner = \"nixos\"").unwrap();
        let err = check(&f, &Catalog::builtin()).unwrap_err();
        assert!(
            err.contains("flock.toml: machine m: provisioner \"nixos\" is not available"),
            "{err}"
        );
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let dir = paths.provisioners_dir().join("nixos");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(crate::provision::MANIFEST_FILE),
            "id = \"nixos\"\nversion = \"0.1.0\"\n[platform]\nos = \"linux\"\ndistro = [\"nixos\"]\n[[check]]\nid = \"nix\"\ncommand = [\"sh\", \"-c\", \"command -v nix\"]\n",
        )
        .unwrap();
        check(&f, &Catalog::load(&paths).unwrap()).unwrap();
        // A machine with none is fine with any catalog.
        check(&flock("").unwrap(), &Catalog::builtin()).unwrap();
    }

    /// `accounts` on a flock takes `github` and `claude`, each once.
    #[test]
    fn flock_accounts_are_github_and_claude() {
        use crate::provision::Account;
        let flock = |extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!("[[flock]]\nname = \"p\"\ndefault = true\n{extra}\n"),
            )
        };
        let f = flock("accounts = [\"github\", \"claude\"]").unwrap();
        let kinds = |f: &Flock| f.flocks[0].accounts.iter().copied().collect::<Vec<_>>();
        assert_eq!(kinds(&f), [Account::Github, Account::Claude]);
        let f = flock("accounts = [\"claude\"]").unwrap();
        assert_eq!(kinds(&f), [Account::Claude]);
        assert!(flock("").unwrap().flocks[0].accounts.is_empty());
        let err = format!("{:#}", flock("accounts = [\"slack\"]").unwrap_err());
        assert!(
            err.contains("slack") && err.contains("github") && err.contains("claude"),
            "{err}"
        );
        let err = format!(
            "{:#}",
            flock("accounts = [\"github\", \"github\"]").unwrap_err()
        );
        assert!(
            err.contains("flock p: accounts: github is listed twice"),
            "{err}"
        );
        let err = format!("{:#}", flock("accounts = \"github\"").unwrap_err());
        assert!(err.contains("accounts"), "{err}");
        // Round trip: what `save` writes loads back the same.
        let text = toml::to_string_pretty(&f).unwrap();
        assert!(text.contains("accounts = [\"claude\"]"), "{text}");
        assert_eq!(Flock::parse(Path::new("flock.toml"), &text).unwrap(), f);
    }

    /// `accounts` as a map names the account `machine check` expects for
    /// each kind; the list form names none, and both load, save and load
    /// back the same.
    #[test]
    fn flock_accounts_as_a_map_name_the_expected_account() {
        use crate::provision::Account;
        let flock = |extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!("[[flock]]\nname = \"p\"\ndefault = true\n{extra}\n"),
            )
        };
        let f = flock("accounts = { github = \"alice\", claude = \"alice@example.com\" }").unwrap();
        let a = &f.flocks[0].accounts;
        assert_eq!(a.name(Account::Github), Some("alice"));
        assert_eq!(a.name(Account::Claude), Some("alice@example.com"));
        assert_eq!(a.iter().count(), 2);
        // One kind named: the other is not an account of the flock.
        let f = flock("accounts = { claude = \"alice@example.com\" }").unwrap();
        assert!(!f.flocks[0].accounts.contains(&Account::Github));
        assert_eq!(f.flocks[0].accounts.name(Account::Github), None);
        // The list form has no names.
        let old = flock("accounts = [\"github\", \"claude\"]").unwrap();
        assert_eq!(old.flocks[0].accounts.name(Account::Github), None);
        assert!(old.flocks[0].accounts.contains(&Account::Github));
        // An unknown kind, a name that is not text and an empty name.
        for bad in [
            "accounts = { slack = \"x\" }",
            "accounts = { github = 3 }",
            "accounts = { github = \" \" }",
        ] {
            assert!(flock(bad).is_err(), "{bad}");
        }
        let err = format!("{:#}", flock("accounts = { github = \"\" }").unwrap_err());
        assert!(
            err.contains("flock p: accounts: the name for github is empty"),
            "{err}"
        );
        // Round trip, in the form it was written.
        let text = toml::to_string_pretty(&f).unwrap();
        assert!(text.contains("claude = \"alice@example.com\""), "{text}");
        assert_eq!(Flock::parse(Path::new("flock.toml"), &text).unwrap(), f);
    }

    /// `reserved` on a machine keeps agent slots free of pastor's own tasks:
    /// 0 by default and not written then, up to `max_agents`, and more than
    /// that fails the load with the machine and the numbers.
    #[test]
    fn a_machine_may_reserve_slots_up_to_max_agents() {
        let load = |extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!(
                    "[[machine]]\nname = \"pi-3\"\nssh = \"u@pi-3\"\nmax_agents = 3\n{extra}\n"
                ),
            )
        };
        let f = load("").unwrap();
        assert_eq!(f.get("pi-3").unwrap().reserved, 0);
        assert!(!toml::to_string(&f).unwrap().contains("reserved"));
        let f = load("reserved = 2").unwrap();
        assert_eq!(f.get("pi-3").unwrap().reserved, 2);
        let text = toml::to_string(&f).unwrap();
        assert!(text.contains("reserved = 2"), "{text}");
        assert_eq!(Flock::parse(Path::new("flock.toml"), &text).unwrap(), f);
        assert_eq!(
            load("reserved = 3").unwrap().get("pi-3").unwrap().reserved,
            3
        );
        let err = format!("{:#}", load("reserved = 4").unwrap_err());
        assert!(
            err.contains("machine pi-3: reserved 4 is more than max_agents 3"),
            "{err}"
        );
        assert!(load("reserved = -1").is_err());
    }

    /// `bundles`, `bundle_vars` and `bundle_apply` on a flock, `bundles`
    /// and `bundle_apply` on a machine: their syntax at load, and a round
    /// trip.
    #[test]
    fn bundle_keys_load_and_are_checked() {
        use crate::bundle::Apply;
        let flock = |flock_extra: &str, machine_extra: &str| {
            Flock::parse(
                Path::new("flock.toml"),
                &format!(
                    "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = {{ build-01 = 2 }}\n{flock_extra}\n\
                     [[machine]]\nname = \"build-01\"\nssh = \"u@b\"\n{machine_extra}\n"
                ),
            )
        };
        let f = flock(
            "bundles = [\"base\", \"team-rules\"]\nbundle_vars = { ticket_url = \"https://tickets.example.com\" }\nbundle_apply = \"auto\"",
            "bundles = [\"gpu-tools\"]\nbundle_apply = \"manual\"",
        )
        .unwrap();
        assert_eq!(f.flocks[0].bundles, ["base", "team-rules"]);
        assert_eq!(
            f.flocks[0].bundle_vars["ticket_url"],
            "https://tickets.example.com"
        );
        assert_eq!(f.flocks[0].bundle_apply, Some(Apply::Auto));
        assert_eq!(f.machines[0].bundles, ["gpu-tools"]);
        assert_eq!(f.machines[0].bundle_apply, Some(Apply::Manual));
        let text = toml::to_string_pretty(&f).unwrap();
        assert_eq!(Flock::parse(Path::new("flock.toml"), &text).unwrap(), f);

        let bad: &[(&str, &str, &str)] = &[
            (
                "bundles = [\"Base\"]",
                "",
                "flock ops: bundles: id \"Base\" must match",
            ),
            (
                "bundles = [\"a\", \"a\"]",
                "",
                "flock ops: bundles: a is listed twice",
            ),
            (
                "bundle_vars = { Url = \"x\" }",
                "",
                "flock ops: bundle_vars: \"Url\" must match",
            ),
            ("bundle_apply = \"sometimes\"", "", "unknown variant"),
            (
                "",
                "bundles = [\"a b\"]",
                "machine build-01: bundles: id \"a b\" must match",
            ),
            ("", "bundle_apply = \"always\"", "unknown variant"),
        ];
        for (fx, mx, want) in bad {
            let err = format!("{:#}", flock(fx, mx).unwrap_err());
            assert!(err.contains(want), "{fx} {mx}\n=> {err}");
        }
    }

    /// A machine gets its flocks' bundles in file order, then its own, each
    /// once; it applies as its own `bundle_apply` says, else as its flocks
    /// agree, else `manual`.
    #[test]
    fn a_machine_gets_its_flocks_bundles_and_apply_mode() {
        use crate::bundle::Apply;
        let f = Flock::parse(
            Path::new("flock.toml"),
            "[[flock]]\nname = \"ops\"\ndefault = true\nbundles = [\"base\", \"team-rules\"]\nbundle_apply = \"auto\"\n\
             machines = { a = 1, b = 1, c = 1 }\n\
             [[flock]]\nname = \"desk\"\nbundles = [\"base\", \"laptop\"]\nmachines = { b = 1 }\n\
             [[machine]]\nname = \"a\"\nssh = \"u@a\"\nbundles = [\"gpu-tools\"]\n\
             [[machine]]\nname = \"b\"\nssh = \"u@b\"\n\
             [[machine]]\nname = \"c\"\nssh = \"u@c\"\nbundle_apply = \"manual\"\n",
        )
        .unwrap();
        let m = |n: &str| f.get(n).unwrap();
        assert_eq!(f.bundles_of(m("a")), ["base", "team-rules", "gpu-tools"]);
        assert_eq!(f.bundles_of(m("b")), ["base", "team-rules", "laptop"]);
        assert_eq!(f.bundle_apply(m("a")), Apply::Auto);
        assert_eq!(
            f.bundle_apply(m("b")),
            Apply::Manual,
            "ops says auto, desk nothing"
        );
        assert_eq!(f.bundle_apply(m("c")), Apply::Manual);
        let users = f.bundle_users("base");
        assert_eq!(users.flocks, ["ops", "desk"]);
        assert!(users.machines.is_empty());
        assert_eq!(users.affected, ["a", "b", "c"]);
        let users = f.bundle_users("gpu-tools");
        assert_eq!(
            (users.flocks.len(), users.machines.as_slice()),
            (0, &["a".to_string()][..])
        );
        assert!(f.bundle_users("nobody").is_empty());
    }

    /// A flock's `workdir_bundles` names workdir bundles, a home bundle
    /// goes in `bundles`, and each list holds only its own target; the
    /// flock gives the vars of both. A machine has no `workdir_bundles`.
    #[test]
    fn workdir_bundles_name_workdir_bundles_only() {
        use crate::bundle::{Bundle, Catalog, Manifest};
        let bundle = |id: &str, target: &str, vars: &[&str]| {
            let vars: String = vars.iter().map(|v| format!("{v} = {{}}\n")).collect();
            let item = if target == "workdir" {
                "[[file]]\nsrc = \"f\"\npath = \"x\"\n"
            } else {
                "[[require]]\nid = \"r\"\ncommand = [\"true\"]\n"
            };
            Bundle {
                id: id.into(),
                dir: format!("/bundles/{id}").into(),
                manifest: Manifest::parse(&format!(
                    "id = \"{id}\"\nversion = \"1\"\ntarget = \"{target}\"\n[vars]\n{vars}{item}"
                ))
                .unwrap(),
            }
        };
        let catalog = Catalog::of(vec![
            bundle("base", "home", &[]),
            bundle("repo-notes", "workdir", &["ticket_url"]),
        ]);
        let check = |text: &str| {
            Flock::parse(Path::new("flock.toml"), text)
                .map_err(|e| format!("{e:#}"))?
                .check_bundles(&catalog)
                .map_err(|e| format!("{e:#}"))
        };
        let ok = String::from(
            "[[flock]]\nname = \"ops\"\ndefault = true\nbundles = [\"base\"]\nworkdir_bundles = [\"repo-notes\"]\n\
             bundle_vars = { ticket_url = \"https://t.example.com\" }\nmachines = { a = 1 }\n\
             [[machine]]\nname = \"a\"\nssh = \"u@a\"\n",
        );
        check(&ok).unwrap();
        let parsed = Flock::parse(Path::new("flock.toml"), &ok).unwrap();
        assert_eq!(parsed.flocks[0].workdir_bundles, ["repo-notes"]);
        assert_eq!(parsed.bundle_users("repo-notes").flocks, ["ops"]);
        assert!(
            parsed.bundles_of(parsed.get("a").unwrap()) == ["base"],
            "a workdir bundle reaches no home"
        );
        let cases: &[(String, &str)] = &[
            (
                ok.replace(
                    "workdir_bundles = [\"repo-notes\"]",
                    "workdir_bundles = [\"base\"]",
                ),
                "flock ops: bundle base is a home bundle: name it in bundles, not workdir_bundles",
            ),
            (
                ok.replace(
                    "bundles = [\"base\"]\n",
                    "bundles = [\"base\", \"repo-notes\"]\n",
                ),
                "flock ops: bundle repo-notes goes into tasks' working directories: name it in workdir_bundles",
            ),
            (
                ok.replace(
                    "bundle_vars = { ticket_url = \"https://t.example.com\" }",
                    "",
                ),
                "flock ops: bundle repo-notes needs bundle_vars ticket_url",
            ),
            (
                format!("{ok}bundles = [\"repo-notes\"]\n"),
                "machine a: bundle repo-notes goes into tasks' working directories",
            ),
            (
                ok.replace("\"repo-notes\"]", "\"repo-notes\", \"repo-notes\"]"),
                "flock ops: workdir_bundles: repo-notes is listed twice",
            ),
            (
                format!("{ok}workdir_bundles = [\"repo-notes\"]\n"),
                "unknown field `workdir_bundles`",
            ),
        ];
        for (text, want) in cases {
            let err = check(text).unwrap_err();
            assert!(err.contains(want), "{text}\n=> {err}");
        }
    }

    /// The bundles a flock or machine names must be in the bundles dir,
    /// and the vars must fit them.
    #[test]
    fn check_bundles_wants_the_bundles_and_their_vars() {
        use crate::bundle::{Bundle, Catalog, Manifest};
        let bundle = |id: &str, vars: &[&str]| {
            let vars: String = vars.iter().map(|v| format!("{v} = {{}}\n")).collect();
            Bundle {
                id: id.into(),
                dir: format!("/bundles/{id}").into(),
                manifest: Manifest::parse(&format!(
                    "id = \"{id}\"\nversion = \"1\"\n[vars]\n{vars}[[require]]\nid = \"r\"\ncommand = [\"true\"]\n"
                ))
                .unwrap(),
            }
        };
        let catalog = Catalog::of(vec![
            bundle("base", &[]),
            bundle("team-rules", &["ticket_url"]),
            bundle("gpu-tools", &["driver"]),
        ]);
        let check = |text: &str| {
            Flock::parse(Path::new("flock.toml"), text)
                .unwrap()
                .check_bundles(&catalog)
                .map_err(|e| format!("{e:#}"))
        };
        let machines =
            "[[machine]]\nname = \"a\"\nssh = \"u@a\"\n[[machine]]\nname = \"b\"\nssh = \"u@b\"\n";
        let ok = format!(
            "[[flock]]\nname = \"ops\"\ndefault = true\nbundles = [\"base\", \"team-rules\"]\n\
             bundle_vars = {{ ticket_url = \"https://t.example.com\" }}\nmachines = {{ a = 1 }}\n{machines}"
        );
        check(&ok).unwrap();
        let cases: &[(String, &str)] = &[
            (
                ok.replace("\"team-rules\"]", "\"team-rules\", \"absent\"]"),
                "flock ops: bundle \"absent\" is not in the bundles dir",
            ),
            (
                ok.replace(
                    "bundle_vars = { ticket_url = \"https://t.example.com\" }",
                    "",
                ),
                "flock ops: bundle team-rules needs bundle_vars ticket_url",
            ),
            (
                format!("{ok}bundles = [\"gpu-tools\"]\n"),
                "machine b: bundle gpu-tools needs bundle_vars driver, in one of its flocks",
            ),
            // Every problem is named, not only the first.
            (
                format!("{ok}bundles = [\"absent\", \"gpu-tools\"]\n"),
                "machine b: bundle \"absent\" is not in the bundles dir; add it with `pastor bundle add`; \
                 machine b: bundle gpu-tools needs bundle_vars driver, in one of its flocks",
            ),
        ];
        for (text, want) in cases {
            let err = check(text).unwrap_err();
            assert!(err.contains(want), "{text}\n=> {err}");
        }
        // A var no bundle declares yet is let be: set before the version
        // that needs it is added.
        check(&ok.replace("ticket_url = ", "driver = \"x\", ticket_url = ")).unwrap();
        // A machine's var comes from any of its flocks, and a flock may give
        // one for its machines' own bundles; two flocks may not differ.
        let two = String::from(
            "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = { a = 1 }\nbundle_vars = { driver = \"x\" }\n\
             [[flock]]\nname = \"lab\"\nmachines = { a = 1 }\nbundle_vars = { driver = \"x\" }\n\
             [[machine]]\nname = \"a\"\nssh = \"u@a\"\nbundles = [\"gpu-tools\"]\n",
        );
        check(&two).unwrap();
        let err = check(&two.replacen("driver = \"x\"", "driver = \"y\"", 1)).unwrap_err();
        assert!(
            err.contains(
                "machine a: bundle_vars driver is \"y\" in flock ops and \"x\" in flock lab"
            ),
            "{err}"
        );
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let mut f = Flock::default();
        f.add(pi("pi-3")).unwrap();
        f.save(&path).unwrap();
        assert_eq!(Flock::load(&path).unwrap(), f);
    }

    #[test]
    fn validate_rejects_bad_machines() {
        let mut f = Flock::default();
        f.machines.push(MachineConfig {
            local: true,
            ..pi("both")
        });
        assert!(f.validate().unwrap_err().contains("both"));
        let mut f = Flock::default();
        f.machines.push(MachineConfig {
            ssh: None,
            ..pi("none")
        });
        assert!(f.validate().unwrap_err().contains("none"));
        let mut f = Flock::default();
        f.machines.push(pi("dup"));
        f.machines.push(pi("dup"));
        assert!(f.validate().unwrap_err().contains("dup"));
        let mut f = Flock::default();
        f.machines.push(MachineConfig {
            max_agents: 0,
            ..pi("zero")
        });
        assert!(f.validate().unwrap_err().contains("zero"));
    }

    /// `env` on a flock and on a machine: read, kept in a round trip, and
    /// checked at load: a name that is no variable name, or a value that
    /// looks like a secret, fails naming the flock or machine and the key,
    /// never the value.
    #[test]
    fn env_on_a_flock_and_a_machine_is_read_and_checked() {
        let text = "[[flock]]\nname = \"work\"\ndefault = true\nmachines = { pi-3 = 1 }\n\
             env = { NOTES_URL = \"http://notes.example:8790\" }\n\
             [[machine]]\nname = \"pi-3\"\nssh = \"a@pi-3\"\nenv = { CARGO_BUILD_JOBS = \"2\" }\n";
        let f = Flock::parse(std::path::Path::new("flock.toml"), text).unwrap();
        assert_eq!(
            f.entry("work").unwrap().env["NOTES_URL"],
            "http://notes.example:8790"
        );
        assert_eq!(f.get("pi-3").unwrap().env["CARGO_BUILD_JOBS"], "2");
        let again = Flock::parse(
            std::path::Path::new("flock.toml"),
            &toml::to_string(&f).unwrap(),
        )
        .unwrap();
        assert_eq!(again, f);
        assert!(!toml::to_string(&Flock::default()).unwrap().contains("env"));

        let secret = format!("gh{}_{}", "p", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cF0hJ3kL6");
        for (flock_env, machine_env, says) in [
            (
                "env = { \"1BAD\" = \"x\" }\n",
                "",
                "flock work: env: \"1BAD\" is not a variable name",
            ),
            (
                "",
                "env = { \"A-B\" = \"x\" }\n",
                "machine pi-3: env: \"A-B\" is not a variable name",
            ),
            (
                &format!("env = {{ GH = \"{secret}\" }}\n"),
                "",
                "flock work: env: GH holds a GitHub token",
            ),
            (
                "",
                &format!("env = {{ GH = \"{secret}\" }}\n"),
                "machine pi-3: env: GH holds a GitHub token",
            ),
        ] {
            let text = format!(
                "[[flock]]\nname = \"work\"\ndefault = true\nmachines = {{ pi-3 = 1 }}\n{flock_env}\
                 [[machine]]\nname = \"pi-3\"\nssh = \"a@pi-3\"\n{machine_env}"
            );
            let err = Flock::parse(std::path::Path::new("flock.toml"), &text).unwrap_err();
            let said = format!("{err:#}");
            assert!(said.contains(says), "{said}");
            assert!(!said.contains(&secret), "the value is never said: {said}");
        }
    }

    /// `pins` on a flock and on a machine: read, kept in a round trip,
    /// checked at load, and the machine's own replace its flock's per kind.
    #[test]
    fn pins_are_read_checked_and_overridden_by_the_machine() {
        let text = "[[flock]]\nname = \"work\"\ndefault = true\nmachines = { pi-3 = 1, pi-4 = 1 }\n\
             pins = { claude = \"2.1.310\", agy = \"1.4\" }\n\
             [[machine]]\nname = \"pi-3\"\nssh = \"a@pi-3\"\n\
             [[machine]]\nname = \"pi-4\"\nssh = \"a@pi-4\"\npins = { claude = \"2.1.309\" }\n";
        let f = Flock::parse(std::path::Path::new("flock.toml"), text).unwrap();
        let pins = |m: &str| -> Vec<(String, String)> {
            f.pins_for(f.get(m).unwrap()).into_iter().collect()
        };
        assert_eq!(
            pins("pi-3"),
            [
                ("agy".into(), "1.4".into()),
                ("claude".into(), "2.1.310".into())
            ]
        );
        assert_eq!(
            pins("pi-4"),
            [
                ("agy".into(), "1.4".into()),
                ("claude".into(), "2.1.309".into())
            ]
        );
        let again = Flock::parse(
            std::path::Path::new("flock.toml"),
            &toml::to_string(&f).unwrap(),
        )
        .unwrap();
        assert_eq!(again, f);
        // No pins is none, and writes none.
        assert!(!toml::to_string(&Flock::default()).unwrap().contains("pins"));

        for (bad, says) in [
            ("pins = { codex = \"1.0\" }\n", "flock work: pins.codex"),
            (
                "pins = { claude = \"newest\" }\n",
                "flock work: pins.claude",
            ),
            ("pins = { claude = \"\" }\n", "is not a version"),
        ] {
            let text = format!(
                "[[flock]]\nname = \"work\"\ndefault = true\n{bad}\
                 [[machine]]\nname = \"pi-3\"\nssh = \"a@pi-3\"\n"
            );
            let err = Flock::parse(std::path::Path::new("flock.toml"), &text).unwrap_err();
            assert!(format!("{err:#}").contains(says), "{bad}: {err:#}");
        }
        let text = "[[machine]]\nname = \"pi-3\"\nssh = \"a@pi-3\"\npins = { opencode = \"1\" }\n";
        let err = Flock::parse(std::path::Path::new("flock.toml"), text).unwrap_err();
        assert!(
            format!("{err:#}").contains("machine pi-3: pins.opencode"),
            "{err:#}"
        );
    }

    #[test]
    fn add_rejects_duplicate_and_remove_reports() {
        let mut f = Flock::default();
        f.add(pi("a")).unwrap();
        assert!(f.add(pi("a")).is_err());
        assert!(f.remove("a"));
        assert!(!f.remove("a"));
    }

    #[test]
    fn add_rejects_invalid_machine_without_mutating() {
        let mut f = Flock::default();
        assert!(
            f.add(MachineConfig {
                local: true,
                ..pi("both")
            })
            .is_err()
        );
        assert!(f.machines.is_empty());

        let mut f = Flock::default();
        assert!(
            f.add(MachineConfig {
                max_agents: 0,
                ..pi("zero")
            })
            .is_err()
        );
        assert!(f.machines.is_empty());
    }

    /// Every char `pred` holds for, up to U+3000 where Unicode's last
    /// whitespace sits, so a strategy can pick one without filtering.
    fn chars_where(pred: fn(char) -> bool) -> Vec<char> {
        (0..=0x3000)
            .filter_map(char::from_u32)
            .filter(|c| pred(*c))
            .collect()
    }

    /// `s` with `c` put in at the char position `at` (modulo its length).
    fn insert_at(s: &str, at: usize, c: char) -> String {
        let n = s.chars().count();
        let at = at % (n + 1);
        s.chars()
            .take(at)
            .chain([c])
            .chain(s.chars().skip(at))
            .collect()
    }

    proptest::proptest! {
        /// A target that ssh would read as an option is never accepted.
        #[test]
        fn prop_ssh_target_with_a_dash_prefix_is_refused(rest in ".*") {
            let target = format!("-{rest}");
            proptest::prop_assert!(ssh_target_problem(&target).is_some(), "{target:?}");
        }

        #[test]
        fn prop_ssh_target_with_whitespace_is_refused(
            s in ".*",
            at in proptest::prelude::any::<usize>(),
            c in proptest::sample::select(chars_where(char::is_whitespace)),
        ) {
            let target = insert_at(&s, at, c);
            proptest::prop_assert!(ssh_target_problem(&target).is_some(), "{target:?}");
        }

        #[test]
        fn prop_ssh_target_with_a_control_char_is_refused(
            s in ".*",
            at in proptest::prelude::any::<usize>(),
            c in proptest::sample::select(chars_where(char::is_control)),
        ) {
            let target = insert_at(&s, at, c);
            proptest::prop_assert!(ssh_target_problem(&target).is_some(), "{target:?}");
        }

        #[test]
        fn prop_plain_user_at_host_is_accepted(
            target in "([a-z_][a-z0-9_.-]{0,31}@)?[a-zA-Z0-9][a-zA-Z0-9.-]{0,62}",
        ) {
            proptest::prop_assert_eq!(ssh_target_problem(&target), None, "{:?}", target);
        }
    }
}
