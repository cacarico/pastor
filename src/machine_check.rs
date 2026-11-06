//! `pastor machine check`: what a machine is missing for the agents that
//! can land on it, from its provisioner's checks. It changes nothing on the
//! machine but the run directory `provision::run` makes and removes.
//!
//! The provisioner is the one asked for, else the machine's `provisioner`,
//! else the first in the catalog whose `[platform]` and `detect` fit what
//! the machine says it is. A check for an agent kind runs once per agent of
//! that kind a task can get there, with that agent's env, so two Claude
//! logins give two rows.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::account_rules;
use crate::cli::CliError;
use crate::config::flock::{Flock, MachineConfig};
use crate::config::profile::Profiles;
use crate::config::{AgentChoice, AgentPick, AgentRefusal, Agents, Defaults, Layer, Models};
use crate::harness;
use crate::provision::run::{self, AgentEnv, Row, Shell, Status};
use crate::provision::{Account, Catalog, Provisioner};

mod pane_env;
pub use pane_env::PANE_ENV_ROW;

/// The id of the row a machine gets when no provisioner fits it.
pub const PROVISIONER_NONE_FITS: &str = "provisioner_none_fits";

/// The id of the row that says why a provisioner was picked for a machine
/// that is only like the distribution it is for.
pub const PROVISIONER_PICKED: &str = "provisioner";

/// The code of `--provisioner` naming none the catalog has.
pub const UNKNOWN_PROVISIONER: &str = "unknown_provisioner";

/// The event a head raises for each failed row.
pub const CHECK_FAILED: &str = "machine.check_failed";

/// How long the CLI waits for a head's check: the checks run one after
/// another, each within its own timeout.
pub const WAIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// The id of the row for the PATH pastor's commands on a machine see.
pub const PATH_ROW: &str = "path";

/// The id of the row for the machine reaching the head over ssh.
pub const REACH_ROW: &str = "reach";

/// Printed before the reach probe's answer.
const REACH_MARK: &str = "pastor-reach-2d7f";

/// From the machine, one ping through the same door its agents use: ssh to
/// `head` with no terminal and no prompt, which the head's authorized_keys
/// answers with `pastor bridge --agent`. Prints `ssh <exit status>`, then
/// what came back, so the row can tell an answer from ssh's own complaint.
fn reach_script(head: &str) -> String {
    format!(
        r#"PATH="$HOME/.local/bin:$PATH"; export PATH; printf '%s\n' {REACH_MARK}; out=$(printf '%s\n' '{{"op":"ping"}}' | ssh -o BatchMode=yes -o ConnectTimeout=8 -- {head} 2>&1); printf 'ssh %s\n' "$?"; printf '%s\n' "$out" | head -n 6"#,
        head = crate::herdr::transport::shell_quote(head),
    )
}

/// The `reach` row: a machine's agents can ask the head by ssh. `ok` when
/// the head answers a ping; `fail` naming the destination and ssh's own
/// words when it does not (no key accepted, host key not known, no route).
async fn reach_row(shell: &dyn Shell, machine: &str, head: &str) -> Row {
    let row = |status, first_line: String, reason| Row {
        id: REACH_ROW.into(),
        agent: None,
        status,
        first_line,
        reason,
    };
    let ran = shell
        .run(
            reach_script(head),
            Vec::new(),
            std::time::Duration::from_secs(40),
        )
        .await;
    let ran = match ran {
        Ok(ran) => ran,
        Err(why) => {
            return row(
                Status::Error,
                format!("could not try ssh to {head}"),
                Some(why),
            );
        }
    };
    let stdout = String::from_utf8_lossy(&ran.stdout).to_string();
    let Some((_, answer)) = stdout.split_once(&format!("{REACH_MARK}\n")) else {
        return row(
            Status::Error,
            format!("could not try ssh to {head}"),
            Some(format!(
                "the machine gave no answer ({}){}",
                run::exit(ran.code),
                run::stderr_tail(&ran)
            )),
        );
    };
    let status = answer.lines().next().unwrap_or_default();
    let said: Vec<&str> = answer
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if status == "ssh 0" && said.iter().any(|l| l.contains("\"kind\":\"pong\"")) {
        return row(
            Status::Pass,
            format!("{machine} reaches the head at {head}"),
            None,
        );
    }
    let why = said
        .iter()
        .rev()
        .take(3)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("; ");
    let why: String = why.chars().take(400).collect();
    row(
        Status::Fail,
        format!("ssh to {head} from {machine} failed"),
        Some(if why.is_empty() {
            format!(
                "{status} and no output; add the machine's agent key to the head with `pastor machine authorized-key {machine} --key FILE --write`"
            )
        } else {
            format!(
                "{why}; the head must accept the machine's agent key (`pastor machine authorized-key {machine} --key FILE --write`)"
            )
        }),
    )
}

/// The id of the rows for `gh`'s login on each host, and its token's expiry.
pub const GH_AUTH_ROW: &str = "gh-auth";

/// A token that expires within this many days gets a warning.
const GH_EXPIRY_WARN_DAYS: i64 = 7;

/// Printed before the part of the machine's answer pastor reads, so what its
/// rc files print first is skipped.
const PATH_MARK: &str = "pastor-path-7c2d";

/// The `PATH` row: the directories the commands pastor runs on the machine
/// see (ssh's shell is neither a login nor an interactive one, so mise's
/// shims are put first when the machine has them: `transport::MISE_SHIMS`),
/// and whether herdr and the CLI of each harness the machine's agents run
/// resolve. A CLI that is only in a directory pastor adds for its own
/// version checks (`~/.local/bin` and the manifest's `bin_dirs`) is found;
/// one that resolves nowhere warns.
async fn path_row(
    shell: &dyn Shell,
    harnesses: &crate::harness_manifest::Harnesses,
    agents: &[AgentEnv],
) -> Row {
    let mut bins: Vec<String> = vec![crate::harness_install::HERDR.to_string()];
    let mut dirs: Vec<String> = vec!["$HOME/.local/bin".into()];
    for kind in agents
        .iter()
        .map(|a| a.kind.as_str())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let Some(m) = harnesses.get(kind) else {
            continue;
        };
        if let Some(bin) = &m.binary
            && !bins.contains(bin)
        {
            bins.push(bin.clone());
        }
        for d in &m.bin_dirs {
            let d = format!("$HOME/{}", d.trim_start_matches("~/"));
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    let extra: String = dirs.iter().map(|d| format!("\"{d}\":")).collect();
    let list = bins.join(" ");
    let script = format!(
        r#"printf '%s\n' {PATH_MARK}; printf 'path %s\n' "$PATH"; for b in {list}; do p=$(command -v "$b" 2>/dev/null); if [ -n "$p" ]; then printf 'on %s %s\n' "$b" "$p"; else p=$(PATH={extra}"$PATH" command -v "$b" 2>/dev/null); if [ -n "$p" ]; then printf 'via %s %s\n' "$b" "$p"; else printf 'no %s\n' "$b"; fi; fi; done"#
    );
    let row = |status, first_line: String, reason| Row {
        id: PATH_ROW.into(),
        agent: None,
        status,
        first_line,
        reason,
    };
    let ran = shell
        .run(script, Vec::new(), std::time::Duration::from_secs(30))
        .await;
    let ran = match ran {
        Ok(ran) => ran,
        Err(why) => return row(Status::Warn, "could not read the PATH".into(), Some(why)),
    };
    let stdout = String::from_utf8_lossy(&ran.stdout).to_string();
    let Some((_, answer)) = stdout.split_once(&format!("{PATH_MARK}\n")) else {
        return row(
            Status::Warn,
            "the machine gave no answer about its PATH".into(),
            None,
        );
    };
    let (mut path, mut missing, mut via) = (String::new(), Vec::new(), Vec::new());
    for line in answer.lines() {
        if let Some(p) = line.strip_prefix("path ") {
            path = p.to_string();
        } else if let Some(b) = line.strip_prefix("no ") {
            missing.push(b.to_string());
        } else if let Some(rest) = line.strip_prefix("via ") {
            via.push(rest.to_string());
        }
    }
    if missing.is_empty() {
        let mut said = format!("{} found", bins.join(", "));
        if !via.is_empty() {
            let found: Vec<String> = via
                .iter()
                .map(|v| {
                    let (b, p) = v.split_once(' ').unwrap_or((v, ""));
                    format!("{b} at {p}, a directory pastor adds")
                })
                .collect();
            said.push_str(&format!(" ({})", found.join("; ")));
        }
        row(Status::Pass, format!("{said}; PATH {path}"), None)
    } else {
        row(
            Status::Warn,
            format!("{} not found; PATH {path}", missing.join(", ")),
            Some(
                "pastor's commands see this PATH; install it, or give the machine a mise shim for it"
                    .into(),
            ),
        )
    }
}

/// The id of the row for the herdr version against its pin.
pub const HERDR_VERSION_ROW: &str = "herdr-version";

/// The id of the row for a running herdr that is not the one installed.
pub const HERDR_RUNNING_ROW: &str = "herdr-running";

/// The id of the row for a commit made on the machine signing and verifying.
pub const SIGNING_ROW: &str = "signing";

/// The id of the rows for the profile each agent on the machine runs under.
pub const PROFILE_ROW: &str = "profile";

/// The id of the row for what opencode's own config on the machine holds.
pub const OPENCODE_ROW: &str = "opencode-config";

/// How long the machine has to make and verify a signed commit: a signer
/// waiting on a passphrase it can never be given must not hold the check.
const SIGNING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// What decides which agents can land on a machine: `[defaults]`,
/// `[agents]` and `[models]` of `pastor.toml`.
pub struct Config<'a> {
    pub defaults: &'a Defaults,
    pub agents: &'a Agents,
    pub models: &'a Models,
    /// `[profiles]`, to resolve the machine's own profile.
    pub profiles: &'a Profiles,
    /// The harness manifests: how to ask for a version, where a harness
    /// keeps what a bundle places.
    pub harnesses: &'a crate::harness_manifest::Harnesses,
    /// `[artifacts] herdr`: the herdr version every machine should run;
    /// `None` when none is pinned.
    pub herdr_pin: Option<&'a str>,
    /// `head_address` of `pastor.toml`: the ssh destination the agents on
    /// an ssh machine reach the head by. `None` when unset, and then there
    /// is no `reach` row (the unset address is a warning of its own).
    pub head_address: Option<&'a str>,
}

/// A machine check's outcome, as `--json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub machine: String,
    /// The provisioner whose checks ran, and its version; `None` when none
    /// fits the machine.
    pub provisioner: Option<String>,
    pub version: Option<String>,
    /// The agents its kind checks ran for.
    pub agents: Vec<String>,
    pub rows: Vec<Row>,
}

impl Report {
    /// The rows that say the machine is missing something: a check that
    /// failed or could not run. A skipped one follows from one of these.
    pub fn failed(&self) -> impl Iterator<Item = &Row> {
        self.rows
            .iter()
            .filter(|r| matches!(r.status, Status::Fail | Status::Error))
    }

    pub fn ok(&self) -> bool {
        self.failed().next().is_none()
    }

    /// One line for the machine and its provisioner, then one per row.
    pub fn text(&self) -> String {
        let mut out = match (&self.provisioner, &self.version) {
            (Some(p), Some(v)) => format!("{}: provisioner {p} {v}\n", self.machine),
            (Some(p), None) => format!("{}: provisioner {p}\n", self.machine),
            _ => format!("{}: no provisioner\n", self.machine),
        };
        for row in &self.rows {
            let label = match row.status {
                Status::Pass => "ok",
                Status::Fail => "fail",
                Status::Error => "error",
                Status::Skipped => "skip",
                Status::Warn => "warn",
            };
            let mut line = format!("{label:<5}  {}", row.id);
            if let Some(agent) = &row.agent {
                line.push_str(&format!(" ({agent})"));
            }
            let said: Vec<&str> = [Some(row.first_line.as_str()), row.reason.as_deref()]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect();
            if !said.is_empty() {
                line.push_str(&format!("  {}", said.join(": ")));
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// What a `machine.check_failed` event about `row` carries.
    pub fn event_detail(&self, row: &Row) -> serde_json::Value {
        serde_json::json!({
            "check": row.id,
            "agent": row.agent,
            "status": row.status,
            "first_line": row.first_line,
            "reason": row.reason,
            "provisioner": self.provisioner,
        })
    }
}

/// What dispatch picks for a task on `m`: for each of its flocks, the pick
/// for a task that asks for nothing, and for one that asks for each model
/// `[models]` names, with the flock and the ask. One the machine cannot run
/// (its flock's agent is of a kind it has no agent for) is left out.
fn picks_on(
    m: &MachineConfig,
    flock: &Flock,
    config: &Config<'_>,
) -> Vec<(String, AgentChoice, AgentPick)> {
    let asks: Vec<AgentChoice> = std::iter::once(AgentChoice::default())
        .chain(config.models.0.keys().map(|model| AgentChoice {
            model: Some(model.clone()),
            ..AgentChoice::default()
        }))
        .collect();
    let mut out = Vec::new();
    for (name, _) in flock.flocks_of(m) {
        let entry = flock.entry(name);
        for ask in &asks {
            let pick = config.defaults.resolve_agent_for(
                ask,
                Some(m),
                entry,
                config.models,
                config.agents,
            );
            if pick.missing_kind.is_none() {
                out.push((name.to_string(), ask.clone(), pick));
            }
        }
    }
    out
}

/// Every agent a task can get on `m`, as dispatch picks them
/// (`picks_on`), each once.
pub fn agents_on(m: &MachineConfig, flock: &Flock, config: &Config<'_>) -> Vec<AgentEnv> {
    let names: BTreeSet<String> = picks_on(m, flock, config)
        .into_iter()
        .map(|(_, _, pick)| pick.agent)
        .collect();
    names
        .into_iter()
        .map(|name| AgentEnv {
            kind: config.agents.kind(&name).to_string(),
            env: config
                .agents
                .0
                .get(&name)
                .map(|d| d.env.clone())
                .unwrap_or_default(),
            name,
        })
        .collect()
}

/// The `profile` rows: one for each agent and profile a task on `m` can
/// run under, saying where the profile comes from. `ok` when the profile
/// renders for the agent's harness as it would for a task: settled as the
/// head settles one when it queues it (`AgentPick::settle`, with the
/// machine's own profile), then started as dispatch starts it
/// (`Agents::launch`); `fail` with the refusal a task would get when it
/// does not, and the model asked for when only a task for that model
/// would. An agent no profile applies to has no row.
fn profile_rows(m: &MachineConfig, flock: &Flock, config: &Config<'_>) -> Vec<Row> {
    // (agent, profile, where it comes from) and the first refusal, with
    // the model asked for, in the order they come.
    type Refused = Option<(AgentRefusal, Option<String>)>;
    let mut seen: Vec<((String, String, String), Refused)> = Vec::new();
    for (name, ask, pick) in picks_on(m, flock, config) {
        let Some((profile, layer)) = pick.profile.clone() else {
            continue;
        };
        let from = match layer {
            Layer::Machine => format!("machine {}", m.name),
            Layer::Flock => format!("flock {name}"),
            Layer::Defaults => "defaults".to_string(),
            Layer::Ask => "the task".to_string(),
        };
        let entry = flock.entry(&name);
        let own = config.defaults.own_profile(Some(m), entry);
        let mut spec = crate::profile_render::base_spec(&pick.agent);
        pick.apply_to(&mut spec);
        spec.agent_source = Some(Box::new(crate::task::AgentSource {
            profile: Some(profile.clone()),
            ..Default::default()
        }));
        let refused = pick
            .settle(
                config.agents,
                config.models,
                config.profiles,
                own.as_deref(),
                Some(&m.name),
                &mut spec,
            )
            .and_then(|()| config.agents.launch(&spec).map(drop))
            .err()
            .map(|e| (e, ask.model.clone()));
        let key = (pick.agent.clone(), profile, from);
        match seen.iter_mut().find(|(k, _)| *k == key) {
            Some((_, first @ None)) => *first = refused,
            Some(_) => {}
            None => seen.push((key, refused)),
        }
    }
    seen.into_iter()
        .map(|((agent, profile, from), refused)| {
            let kind = config.agents.kind(&agent).to_string();
            let (status, first_line, reason) = match refused {
                None => (
                    Status::Pass,
                    format!("{profile}, from {from}, renders for {kind}"),
                    None,
                ),
                Some((e, model)) => (
                    Status::Fail,
                    format!("{profile}, from {from}, does not render for {kind}"),
                    Some(match model {
                        Some(model) => {
                            format!("{}: {} (a task for model {model})", e.code, e.message)
                        }
                        None => format!("{}: {}", e.code, e.message),
                    }),
                ),
            };
            Row {
                id: PROFILE_ROW.into(),
                agent: Some(agent),
                status,
                first_line,
                reason,
            }
        })
        .collect()
}

/// The `opencode-config` row: what dispatch reads of opencode's config on
/// the machine before it starts an opencode task under a profile. The
/// managed config, and on macOS the managed preferences, are merged after
/// what pastor sends (`opencode::CONFIG_CHECK_COMMAND`), and rules under an
/// agent or a mode in the machine's own config, a file that does not parse
/// or more config than pastor reads are past what it can override
/// (`opencode::CONFIG_READ_COMMAND`): a task under a profile is refused for
/// any of them (`opencode_permissions_conflict`). Then the row fails,
/// naming each file and key, or warns when no opencode agent there runs
/// under a profile, since only a task that asks for one would be refused.
/// Otherwise it is `ok`, naming the top-level keys a task under a profile
/// denies, or `warn` when the machine's answer could not be read, where
/// dispatch goes ahead.
async fn opencode_row(shell: &dyn Shell, profiled: bool) -> Row {
    use crate::config::opencode;
    let row = |status, first_line: String, reason| Row {
        id: OPENCODE_ROW.into(),
        agent: None,
        status,
        first_line,
        reason,
    };
    let time = std::time::Duration::from_secs(30);
    let mut unread: Option<String> = None;
    let mut conflicts: Vec<String> = Vec::new();
    match shell
        .run(opencode::CONFIG_CHECK_COMMAND.into(), Vec::new(), time)
        .await
    {
        Ok(ran) if ran.code == Some(0) => {
            let hits = opencode::parse_config_check(&String::from_utf8_lossy(&ran.stdout));
            if !hits.is_empty() {
                conflicts.push(format!(
                    "has permission rules of its own: {}",
                    opencode::describe_config_check_hits(&hits)
                ));
            }
        }
        Ok(ran) => {
            unread = Some(format!(
                "the managed config check gave {}{}",
                run::exit(ran.code),
                run::stderr_tail(&ran)
            ));
        }
        Err(why) => unread = Some(why),
    }
    let mut keys = Vec::new();
    match shell
        .run(opencode::CONFIG_READ_COMMAND.into(), Vec::new(), time)
        .await
    {
        Ok(ran) => match opencode::parse_permission_keys(&String::from_utf8_lossy(&ran.stdout)) {
            opencode::MachineDenyKeys::Conflict(why) => conflicts.push(why),
            opencode::MachineDenyKeys::Keys(k) => keys = k,
            opencode::MachineDenyKeys::None => {}
        },
        Err(why) => unread = unread.or(Some(why)),
    }
    if !conflicts.is_empty() {
        let (status, refused) = if profiled {
            (Status::Fail, "a task under a profile is refused here")
        } else {
            (
                Status::Warn,
                "a task that asks for a profile is refused here",
            )
        };
        return row(
            status,
            conflicts.join("; "),
            Some(format!(
                "{}: {refused}; move the rules out of its opencode config",
                opencode::OPENCODE_PERMISSIONS_CONFLICT
            )),
        );
    }
    if let Some(why) = unread {
        return row(
            Status::Warn,
            "could not read the opencode config".into(),
            Some(why),
        );
    }
    if keys.is_empty() {
        row(Status::Pass, "no permission rules of its own".into(), None)
    } else {
        row(
            Status::Pass,
            format!(
                "sets {}, which a task under a profile denies",
                keys.join(", ")
            ),
            None,
        )
    }
}

/// The accounts `m`'s flocks ask for, each once.
fn accounts_of(m: &MachineConfig, flock: &Flock) -> Vec<Account> {
    let mut out: Vec<Account> = Vec::new();
    for (name, _) in flock.flocks_of(m) {
        for a in flock
            .entry(name)
            .into_iter()
            .flat_map(|f| f.accounts.iter())
        {
            if !out.contains(a) {
                out.push(*a);
            }
        }
    }
    out
}

/// Does `p`'s `detect` pass on the machine, or has it none? When it does
/// not, `tried` says why.
async fn detect_fits(
    shell: &Arc<dyn Shell>,
    m: &MachineConfig,
    p: &Provisioner,
    tried: &mut Vec<String>,
) -> anyhow::Result<bool> {
    match run::detect(shell.clone(), &m.name, p)
        .await
        .map_err(|e| CliError::err(e.code(), e))?
    {
        None => Ok(true),
        Some(row) if row.status == Status::Pass => Ok(true),
        Some(row) => {
            let why = [Some(row.first_line), row.reason]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(": ");
            let status = match row.status {
                Status::Fail => "exit 1",
                _ => "an error",
            };
            let why = if why.is_empty() {
                String::new()
            } else {
                format!(" ({why})")
            };
            tried.push(format!("{}'s detect gave {status}{why}", p.id));
            Ok(false)
        }
    }
}

/// The provisioner for `m` and, when it fits only through `ID_LIKE`, why; or
/// why none fits it: each one tried, and what did not match.
///
/// The first in the catalog whose `os` and `distro` match the machine's
/// `ID` and whose `detect` passes is the one. Failing that, the same for each
/// word of its `ID_LIKE`, nearest first: Ubuntu says `debian`, so it gets
/// what a Debian machine gets, a Raspberry Pi running it the Pi's.
async fn pick<'a>(
    shell: &Arc<dyn Shell>,
    m: &MachineConfig,
    catalog: &'a Catalog,
    want: Option<&str>,
) -> anyhow::Result<Result<(&'a Provisioner, Option<String>), Vec<String>>> {
    if let Some(id) = want.or(m.provisioner.as_deref()) {
        catalog
            .check(id)
            .map_err(|e| CliError::err(UNKNOWN_PROVISIONER, e))?;
        return Ok(Ok((catalog.get(id).expect("checked above"), None)));
    }
    let facts = run::facts(shell.as_ref(), &m.name)
        .await
        .map_err(|e| CliError::err(e.code(), e))?;
    let mut distro = facts
        .distro
        .clone()
        .unwrap_or_else(|| "no os-release ID".into());
    if !facts.like.is_empty() {
        distro.push_str(&format!(" (ID_LIKE {})", facts.like.join(" ")));
    }
    let mut tried = Vec::new();
    for p in catalog.provisioners() {
        let platform = &p.manifest.platform;
        if platform.os.to_string() != facts.os {
            tried.push(format!("{} is for {}, not {}", p.id, platform.os, facts.os));
            continue;
        }
        if !platform.distro.is_empty()
            && !facts
                .distro
                .as_ref()
                .is_some_and(|d| platform.distro.contains(d))
        {
            tried.push(format!(
                "{} is for {}, not {distro}",
                p.id,
                platform.distro.join(" or ")
            ));
            continue;
        }
        if detect_fits(shell, m, p, &mut tried).await? {
            return Ok(Ok((p, None)));
        }
    }
    for word in &facts.like {
        for p in catalog.provisioners() {
            let platform = &p.manifest.platform;
            let named_by_id = facts
                .distro
                .as_ref()
                .is_some_and(|d| platform.distro.contains(d));
            if platform.os.to_string() != facts.os || !platform.distro.contains(word) || named_by_id
            {
                continue;
            }
            if detect_fits(shell, m, p, &mut tried).await? {
                let because = match facts.distro.as_deref() {
                    Some(id) => format!("os-release ID {id} has ID_LIKE {word}"),
                    None => format!("os-release has no ID and has ID_LIKE {word}"),
                };
                return Ok(Ok((p, Some(format!("{}, because {because}", p.id)))));
            }
        }
    }
    if tried.is_empty() {
        tried.push("the catalog has no provisioner".into());
    }
    Ok(Err(tried))
}

/// A `harness-version` row for each agent of a pinned kind: the version of
/// its CLI on the machine against the pin. `ok` when it satisfies the pin,
/// `warn` naming both when it does not or when the machine's answer is not a
/// version; no row when the CLI is not there (the kind's install row says
/// so) or the pin is `latest`. The version is asked once per kind. A row
/// that drifts names the fix, `pastor machine install` of `machine`.
async fn harness_rows(
    machine: &str,
    shell: &dyn Shell,
    harnesses: &crate::harness_manifest::Harnesses,
    pins: &harness::Pins,
    agents: &[AgentEnv],
) -> Vec<Row> {
    let mut seen: BTreeMap<&str, Result<Option<String>, String>> = BTreeMap::new();
    let mut out = Vec::new();
    for agent in agents {
        let Some(pin) = pins.get(&agent.kind) else {
            continue;
        };
        // A pin on a harness whose manifest reads no version is not checked:
        // say so, once per agent, rather than show nothing.
        let Some((manifest, command)) = harnesses
            .get(&agent.kind)
            .and_then(|m| m.version_command().map(|c| (m, c)))
        else {
            out.push(Row {
                id: harness::VERSION_ROW.into(),
                agent: Some(agent.name.clone()),
                status: Status::Warn,
                first_line: format!(
                    "{} is pinned to {pin}, but its manifest reads no version",
                    agent.kind
                ),
                reason: Some(
                    "give the harness a [version] in its manifest, or drop the pin".into(),
                ),
            });
            continue;
        };
        if !seen.contains_key(agent.kind.as_str()) {
            let asked = shell
                .run(command, Vec::new(), std::time::Duration::from_secs(30))
                .await
                .map(|ran| {
                    let text = String::from_utf8_lossy(&ran.stdout).to_string();
                    let last = text.lines().rev().find(|l| !l.trim().is_empty());
                    match last.map(str::trim) {
                        None | Some("none") => None,
                        Some(_) => Some(manifest.parse_version(&text).unwrap_or_default()),
                    }
                });
            seen.insert(agent.kind.as_str(), asked);
        }
        let (status, first_line, reason) = match &seen[agent.kind.as_str()] {
            Err(why) => (
                Status::Warn,
                format!("could not read the {} version", agent.kind),
                Some(why.clone()),
            ),
            Ok(None) => continue,
            Ok(Some(v)) if v.is_empty() => (
                Status::Warn,
                format!("{} printed no version", agent.kind),
                Some(format!("the flock pins {pin}")),
            ),
            Ok(Some(v)) => {
                let s = harness::HarnessStatus::new(&agent.kind, pin, Some(v.clone()));
                if s.drift {
                    (
                        Status::Warn,
                        s.say(),
                        Some(format!(
                            "fix with `pastor machine install {machine} --kind {}`",
                            agent.kind
                        )),
                    )
                } else if pin == harness::LATEST {
                    continue;
                } else {
                    (Status::Pass, s.say(), None)
                }
            }
        };
        out.push(Row {
            id: harness::VERSION_ROW.into(),
            agent: Some(agent.name.clone()),
            status,
            first_line,
            reason,
        });
    }
    out
}

/// Printed before the `gh` probe's answer.
const GH_MARK: &str = "pastor-gh-4b81";

/// What `gh` says on the machine: one `login` line per account it names
/// (`gh auth status`, cut to the lines that say who is logged in where, and
/// whether the account is the active one), and for each host with a working
/// login the `expires` line of the API's token expiry header, when the API
/// sends one. It prints `nogh` when there is no `gh`. The token is never
/// asked for or printed: `gh auth status` masks it, and only these lines
/// pass.
fn gh_script() -> String {
    format!(
        r#"PATH="$HOME/.local/bin:$PATH"; export PATH; printf '%s\n' {GH_MARK}; command -v gh >/dev/null 2>&1 || {{ echo nogh; exit 0; }}; GH_PROMPT_DISABLED=1; export GH_PROMPT_DISABLED; gh auth status 2>&1 </dev/null | grep -E 'Logged in to|Failed to log in|Active account' | sed 's/^/login /'; for h in $(gh auth status 2>&1 </dev/null | grep -E 'Logged in to' | sed -n 's/.*Logged in to \([^ ]*\).*/\1/p' | sort -u); do e=$(gh api -i --hostname "$h" user 2>/dev/null </dev/null | grep -i '^github-authentication-token-expiration:' | head -1 | sed 's/^[^:]*: *//' | tr -d '\r'); [ -n "$e" ] && printf 'expires %s %s\n' "$h" "$e"; done; true"#
    )
}

/// One account `gh auth status` named.
#[derive(Debug, PartialEq, Eq)]
struct GhLogin {
    host: String,
    account: String,
    ok: bool,
    /// `Some(false)` for an account that is not the one used on its host.
    active: Option<bool>,
}

/// The accounts in `login` lines, in order.
fn gh_logins(answer: &str) -> Vec<GhLogin> {
    let mut out: Vec<GhLogin> = Vec::new();
    for line in answer.lines() {
        let Some(line) = line.strip_prefix("login ") else {
            continue;
        };
        let line = line.trim();
        let ok = line.contains("Logged in to ");
        if let Some((_, rest)) = line
            .split_once("Logged in to ")
            .or_else(|| line.split_once("Failed to log in to "))
        {
            let mut words = rest.split_whitespace();
            let host = words.next().unwrap_or_default().to_string();
            // `account NAME (where)`, or `as NAME (where)` in older gh.
            let account = match words.next() {
                Some("account") | Some("as") => words.next().unwrap_or_default().to_string(),
                _ => String::new(),
            };
            out.push(GhLogin {
                host,
                account,
                ok,
                active: None,
            });
        } else if let Some(active) = line.split_once("Active account:").map(|(_, v)| v.trim())
            && let Some(last) = out.last_mut()
        {
            last.active = Some(active == "true");
        }
    }
    out
}

/// The expiry `gh`'s API reported for `host`, from `expires` lines.
fn gh_expiry(answer: &str, host: &str) -> Option<chrono::NaiveDateTime> {
    answer.lines().find_map(|l| {
        let rest = l.strip_prefix("expires ")?.strip_prefix(host)?;
        let when = rest.strip_prefix(' ')?.trim().trim_end_matches(" UTC");
        chrono::NaiveDateTime::parse_from_str(when, "%Y-%m-%d %H:%M:%S").ok()
    })
}

/// The `gh-auth` rows: one for each host `gh` is logged in to (its active
/// account), from `gh auth status`. `ok` when the login works, saying when
/// its token expires if GitHub sent one; `warn` when that is within a week;
/// `fail` when the token does not work. With no login at all one `warn` row
/// says so (the provisioner's own `gh-login` check fails for github.com).
/// None when `gh` is not on the machine: its install check says so. With
/// `expected` (the flock's `accounts = { github = ... }`), the account
/// signed in to github.com must be one of them, or the row is a `fail`
/// naming both.
async fn gh_auth_rows(
    shell: &dyn Shell,
    machine: &str,
    now: chrono::DateTime<chrono::Utc>,
    expected: &[String],
) -> Vec<Row> {
    let row = |status, first_line: String, reason| Row {
        id: GH_AUTH_ROW.into(),
        agent: None,
        status,
        first_line,
        reason,
    };
    let ran = shell
        .run(gh_script(), Vec::new(), std::time::Duration::from_secs(90))
        .await;
    let ran = match ran {
        Ok(ran) => ran,
        Err(why) => {
            return vec![row(
                Status::Warn,
                "could not read gh's login".into(),
                Some(why),
            )];
        }
    };
    let stdout = String::from_utf8_lossy(&ran.stdout).to_string();
    let Some((_, answer)) = stdout.split_once(&format!("{GH_MARK}\n")) else {
        return vec![row(
            Status::Warn,
            "the machine gave no answer about gh's login".into(),
            None,
        )];
    };
    if answer.lines().next().map(str::trim) == Some("nogh") {
        return vec![];
    }
    let logins: Vec<GhLogin> = gh_logins(answer)
        .into_iter()
        .filter(|l| l.active != Some(false))
        .collect();
    if logins.is_empty() {
        return vec![row(
            Status::Warn,
            "gh is not logged in to any host".into(),
            Some(format!(
                "log in on {machine} with `gh auth login`; a token for one repository is in the recommended setup"
            )),
        )];
    }
    logins
        .iter()
        .map(|l| {
            let who = if l.account.is_empty() {
                l.host.clone()
            } else {
                format!("{} as {}", l.host, l.account)
            };
            if !l.ok {
                return row(
                    Status::Fail,
                    format!("{who}: the token does not work"),
                    Some(format!(
                        "log in again on {machine} with `gh auth login --hostname {}`",
                        l.host
                    )),
                );
            }
            if l.host == "github.com" && !expected.is_empty() {
                if l.account.is_empty() {
                    return row(
                        Status::Warn,
                        format!("{who}: could not tell which account is signed in"),
                        Some(format!("the flock expects {}", expected.join(" or "))),
                    );
                }
                if !expected.iter().any(|e| e.eq_ignore_ascii_case(&l.account)) {
                    return row(
                        Status::Fail,
                        format!("{who}: the flock expects {}", expected.join(" or ")),
                        Some(format!(
                            "sign in to the expected account on {machine} with `gh auth login --hostname {}`",
                            l.host
                        )),
                    );
                }
            }
            let Some(expiry) = gh_expiry(answer, &l.host) else {
                return row(Status::Pass, format!("{who}: logged in"), None);
            };
            let days = (expiry.and_utc() - now).num_days();
            let said = format!(
                "{who}: logged in, the token expires {} ({})",
                expiry.format("%Y-%m-%d"),
                match days {
                    d if d < 0 => "it has".to_string(),
                    0 => "today or tomorrow".to_string(),
                    1 => "in 1 day".to_string(),
                    d => format!("in {d} days"),
                }
            );
            if days < GH_EXPIRY_WARN_DAYS {
                row(
                    Status::Warn,
                    said,
                    Some(format!(
                        "make a new token before then and give it to gh on {machine} with `gh auth login --hostname {} --with-token`",
                        l.host
                    )),
                )
            } else {
                row(Status::Pass, said, None)
            }
        })
        .collect()
}

/// The account names `m`'s flocks expect for `account`
/// (`accounts = { claude = "alice@example.com" }`), each once; none when no
/// flock names one.
fn expected_names(m: &MachineConfig, flock: &Flock, account: Account) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (name, _) in flock.flocks_of(m) {
        if let Some(n) = flock.entry(name).and_then(|f| f.accounts.name(account))
            && !out.iter().any(|o| o.eq_ignore_ascii_case(n))
        {
            out.push(n.trim().to_string());
        }
    }
    out
}

/// The id of the rows for the account each Claude agent is signed in as.
pub const CLAUDE_ACCOUNT_ROW: &str = "claude-account";

/// Printed before the Claude probe's answer.
const CLAUDE_MARK: &str = "pastor-claude-9e3a";

/// `claude auth status --text` under `agent`'s env, printing the `Email:`
/// line it says as `email <address>`; the env's `~` is the machine's home.
/// Nothing but that line passes: the token is not part of that output, and
/// no credential file is opened.
fn claude_script(agent: &run::AgentEnv) -> Option<String> {
    let mut exports = String::new();
    for (key, value) in &agent.env {
        let name_ok = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            return None;
        }
        let value = match value.strip_prefix('~') {
            Some("") => "\"$HOME\"".to_string(),
            Some(rest) if rest.starts_with('/') => {
                format!("\"$HOME\"{}", crate::herdr::transport::shell_quote(rest))
            }
            _ => crate::herdr::transport::shell_quote(value),
        };
        exports.push_str(&format!("{key}={value}; export {key}; "));
    }
    Some(format!(
        r#"PATH="$HOME/.local/bin:$PATH"; export PATH; printf '%s\n' {CLAUDE_MARK}; {exports}claude auth status --text </dev/null 2>&1 | sed -n 's/^Email: *\(.*[^ ]\) *$/email \1/p' | head -n 1; true"#
    ))
}

/// A `claude-account` row for each Claude agent that could land on the
/// machine and whose login `rows` do not already fail: `ok` when it is
/// signed in as one of the `expected` accounts (the email `claude auth
/// status` prints, compared without case), `fail` naming both when it is
/// another, `warn` when the machine does not say who. Asked only when a
/// flock names the account, so a flock that does not gets the rows it had.
async fn claude_account_rows(
    shell: &dyn Shell,
    machine: &str,
    agents: &[run::AgentEnv],
    rows: &[Row],
    expected: &[String],
) -> Vec<Row> {
    let mut out = Vec::new();
    for agent in agents.iter().filter(|a| a.kind == "claude") {
        // Not signed in at all is the login row's to say.
        if rows.iter().any(|r| {
            r.id == "claude-login"
                && r.agent.as_deref() == Some(&agent.name)
                && r.status != Status::Pass
        }) {
            continue;
        }
        let row = |status, first_line: String, reason| Row {
            id: CLAUDE_ACCOUNT_ROW.into(),
            agent: Some(agent.name.clone()),
            status,
            first_line,
            reason,
        };
        let Some(script) = claude_script(agent) else {
            continue;
        };
        let ran = shell
            .run(script, Vec::new(), std::time::Duration::from_secs(60))
            .await;
        let stdout = match ran {
            Ok(ran) => String::from_utf8_lossy(&ran.stdout).to_string(),
            Err(why) => {
                out.push(row(
                    Status::Warn,
                    "could not read the signed-in account".into(),
                    Some(why),
                ));
                continue;
            }
        };
        let email = stdout
            .split_once(&format!("{CLAUDE_MARK}\n"))
            .and_then(|(_, after)| {
                after
                    .lines()
                    .find_map(|l| l.strip_prefix("email "))
                    .map(|e| e.trim().to_string())
            })
            .filter(|e| !e.is_empty());
        out.push(match email {
            None => row(
                Status::Warn,
                "claude does not say which account is signed in".into(),
                Some(format!("the flock expects {}", expected.join(" or "))),
            ),
            Some(email) if expected.iter().any(|e| e.eq_ignore_ascii_case(&email)) => {
                row(Status::Pass, format!("signed in as {email}"), None)
            }
            Some(email) => row(
                Status::Fail,
                format!("signed in as {email}, the flock expects {}", expected.join(" or ")),
                Some(format!(
                    "sign in to the expected account on {machine} with `claude auth login`, under this agent's CLAUDE_CONFIG_DIR"
                )),
            ),
        });
    }
    out
}

/// The command that signs `agent` in on `machine`, run from the head.
fn login_command(machine: &str, agent: &str) -> String {
    format!("pastor login {machine} {agent}")
}

/// The fix for a signed-out agent in a failed `<kind>-login` row: the head
/// signs it in (`pastor login`), when its harness has an interactive login to
/// type. Any other failed row, and a kind with no such login, is left as it
/// was.
pub(crate) fn name_login_commands(
    machine: &str,
    harnesses: &crate::harness_manifest::Harnesses,
    rows: &mut [Row],
) {
    for row in rows.iter_mut().filter(|r| r.status == Status::Fail) {
        let (Some(kind), Some(agent)) = (row.id.strip_suffix("-login"), row.agent.as_deref())
        else {
            continue;
        };
        if harnesses
            .get(kind)
            .and_then(|h| h.login_start.as_ref())
            .is_none()
        {
            continue;
        }
        let fix = format!(
            "sign it in from the head with `{}`",
            login_command(machine, agent)
        );
        row.reason = Some(match row.reason.take() {
            Some(why) => format!("{why}; {fix}"),
            None => fix,
        });
    }
}

/// The `pastor login` commands the failed rows of `report` name, in order.
pub fn login_commands(report: &Report) -> Vec<String> {
    report
        .rows
        .iter()
        .filter(|r| r.status == Status::Fail)
        .filter_map(|r| r.reason.as_deref())
        .filter_map(|why| {
            why.split('`')
                .find(|part| part.starts_with("pastor login "))
        })
        .map(str::to_string)
        .collect()
}

/// The `inbox` rows of `m` (`inbox_check`), for a caller that has its report
/// so far in `rows`. They start a short-lived Claude session on the machine,
/// so `check` does not make them: the callers that run a whole check do. A
/// pull machine, which the head never connects to, has none.
pub async fn inbox_rows(
    m: &MachineConfig,
    flock: &Flock,
    config: &Config<'_>,
    paths: &crate::config::Paths,
    rows: &[Row],
) -> Vec<Row> {
    if m.pull {
        return Vec::new();
    }
    let ep = crate::herdr::transport::Endpoint::from_machine(m, paths);
    inbox_rows_on(&ep, m, flock, config, rows).await
}

/// `inbox_rows` through `conn`.
pub async fn inbox_rows_on(
    conn: &dyn crate::herdr::transport::Connector,
    m: &MachineConfig,
    flock: &Flock,
    config: &Config<'_>,
    rows: &[Row],
) -> Vec<Row> {
    let agents = agents_on(m, flock, config);
    crate::inbox_check::rows(
        conn,
        &m.name,
        config.agents,
        &agents,
        rows,
        crate::inbox_check::Timing::default(),
    )
    .await
}

/// What the machine says of the herdr on its PATH.
enum Installed {
    Version(String),
    /// It printed no version.
    Unversioned,
    Missing,
    /// The machine gave no answer; why.
    Unreadable(String),
}

async fn installed_herdr(shell: &dyn Shell) -> Installed {
    let ran = shell
        .run(
            crate::harness_install::herdr_version_command(),
            Vec::new(),
            std::time::Duration::from_secs(30),
        )
        .await;
    let text = match ran {
        Ok(ran) => String::from_utf8_lossy(&ran.stdout).to_string(),
        Err(why) => return Installed::Unreadable(why),
    };
    let last = text.lines().rev().find(|l| !l.trim().is_empty());
    match last.map(str::trim) {
        None | Some("none") => Installed::Missing,
        Some(_) => crate::harness_install::herdr_version_from(&text)
            .map_or(Installed::Unversioned, Installed::Version),
    }
}

/// The row for the herdr version on the machine against `[artifacts]
/// herdr`: `ok` when it is the pin, `warn` naming both when it is not, when
/// herdr is not there or when the machine's answer is not a version. A
/// warning names the fix, `pastor machine install` of `machine` for herdr.
fn herdr_row(machine: &str, installed: &Installed, pin: &str) -> Row {
    let row = |status, first_line: String, reason| Row {
        id: HERDR_VERSION_ROW.into(),
        agent: None,
        status,
        first_line,
        reason,
    };
    let fix = format!(
        "fix with `pastor machine install {machine} --kind {}`",
        crate::harness_install::HERDR
    );
    match installed {
        Installed::Unreadable(why) => row(
            Status::Warn,
            "could not read the herdr version".into(),
            Some(why.clone()),
        ),
        Installed::Missing => row(
            Status::Warn,
            format!("herdr is not on the machine's PATH, pinned {pin}"),
            Some(fix),
        ),
        Installed::Unversioned => row(
            Status::Warn,
            "herdr printed no version".into(),
            Some(format!("pinned {pin}")),
        ),
        Installed::Version(version) => {
            let status = harness::HarnessStatus::new(
                crate::harness_install::HERDR,
                pin,
                Some(version.clone()),
            );
            if status.drift {
                row(Status::Warn, status.say(), Some(fix))
            } else {
                row(Status::Pass, status.say(), None)
            }
        }
    }
}

/// A `herdr-running` row when the herdr server running on the machine is not
/// the version now on its PATH: an install links the new herdr, but a
/// running server goes on being the old one until it is restarted. None
/// when they are the same, or when either is not known.
fn herdr_running_row(installed: &Installed, running: &str) -> Option<Row> {
    let Installed::Version(version) = installed else {
        return None;
    };
    (version != running).then(|| Row {
        id: HERDR_RUNNING_ROW.into(),
        agent: None,
        status: Status::Warn,
        first_line: format!("herdr {version} is installed, herdr {running} is running"),
        reason: Some(
            "the running herdr keeps its version until it is restarted; restart it when no agent is at work"
                .into(),
        ),
    })
}

/// The version of the herdr server running on `m`, from a ping; None when
/// it does not answer in time.
pub async fn running_herdr(m: &MachineConfig, paths: &crate::config::Paths) -> Option<String> {
    use crate::herdr::client::ConnectorExt;
    let ep = crate::herdr::transport::Endpoint::from_machine(m, paths);
    tokio::time::timeout(std::time::Duration::from_secs(10), ep.ping())
        .await
        .ok()?
        .ok()
        .map(|pong| pong.version)
}

/// Printed before the signing probe's answer, so what the machine's rc
/// files print first is skipped.
const SIGNING_MARK: &str = "pastor-signing-7c2e";

/// Makes a repo in `dir` (a run directory on the machine), commits to it
/// with `-S` as git is configured there, then asks git to verify that commit.
/// It prints `nogit`, or `set` or `unset` (whether git's config names a
/// signing key or `commit.gpgsign`, never which) and then `signfail` and
/// git's words, or `signed` and then `verifyfail` and git's words or
/// `verified`. The keys are git's and gpg's to read; nothing here opens one. The commit runs no hooks, never waits on
/// a prompt and is made in `dir` alone.
fn signing_script(dir: &str) -> String {
    format!(
        r#"PATH="$HOME/.local/bin:$PATH"; export PATH; printf '%s\n' {SIGNING_MARK}; command -v git >/dev/null 2>&1 || {{ echo nogit; exit 0; }}; GIT_TERMINAL_PROMPT=0; export GIT_TERMINAL_PROMPT; cd {dir} || exit 3; if [ -n "$(git config --get user.signingkey 2>/dev/null; git config --get commit.gpgsign 2>/dev/null)" ]; then echo set; else echo unset; fi; out=$(git -c advice.defaultBranchName=false init -q . 2>&1) || {{ echo signfail; printf '%s\n' "$out"; exit 0; }}; out=$(git -c core.hooksPath=/dev/null commit -q -S --allow-empty -m 'pastor signing check' </dev/null 2>&1) || {{ echo signfail; printf '%s\n' "$out"; exit 0; }}; echo signed; out=$(git verify-commit HEAD </dev/null 2>&1) || {{ echo verifyfail; printf '%s\n' "$out"; exit 0; }}; echo verified"#,
        dir = crate::herdr::transport::shell_quote(dir),
    )
}

/// What git said, cut to the last few lines a person can read in a row.
fn git_words(lines: &str) -> Option<String> {
    let words: Vec<&str> = lines
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = &words[words.len().saturating_sub(4)..];
    let said: String = tail.join("; ");
    let said: String = said.chars().take(500).collect();
    (!said.is_empty()).then_some(said)
}

/// The row for a commit on the machine: a throwaway repo in a run directory
/// of its own, never a user's repo, signed with what git there is set up to
/// sign with and checked with `git verify-commit`. `ok` when both work;
/// `fail` when git is not there, the commit does not sign (git's words are
/// the reason) or a signed commit does not verify. A commit that does not
/// sign where git's config names no signing key and leaves `commit.gpgsign`
/// unset is a `warn`: signing was never set up, which is no breakage.
async fn signing_row(shell: Arc<dyn Shell>, machine: &str, time: std::time::Duration) -> Row {
    let row = |status, first_line: &str, reason| Row {
        id: SIGNING_ROW.into(),
        agent: None,
        status,
        first_line: first_line.into(),
        reason,
    };
    let dir = match run::empty_dir(shell.clone(), machine).await {
        Ok(dir) => dir,
        Err(e) => {
            return row(
                Status::Error,
                "could not make a repo to sign in",
                Some(e.to_string()),
            );
        }
    };
    let ran = shell
        .run(signing_script(dir.path()), Vec::new(), time)
        .await;
    dir.remove().await;
    let ran = match ran {
        Ok(ran) => ran,
        Err(why) => {
            return row(
                Status::Error,
                "could not try a signed commit",
                Some(format!(
                    "{why}; a signer waiting for a passphrase it cannot be given looks like this"
                )),
            );
        }
    };
    let stdout = String::from_utf8_lossy(&ran.stdout).to_string();
    let Some(answer) = stdout
        .split_once(SIGNING_MARK)
        .map(|(_, after)| after.trim_start_matches(['\r', '\n']))
    else {
        return row(
            Status::Error,
            "could not try a signed commit",
            Some(format!(
                "the machine gave no answer ({}){}",
                run::exit(ran.code),
                run::stderr_tail(&ran)
            )),
        );
    };
    let mut lines = answer.lines();
    let configured = match lines.next().map(str::trim) {
        Some("nogit") => {
            return row(
                Status::Fail,
                "git is not on the machine's PATH",
                Some("agents commit with git; install it".into()),
            );
        }
        Some("set") => true,
        Some("unset") => false,
        _ => {
            return row(
                Status::Error,
                "could not try a signed commit",
                Some(format!(
                    "the machine's answer was not understood ({}){}",
                    run::exit(ran.code),
                    run::stderr_tail(&ran)
                )),
            );
        }
    };
    match lines.next().map(str::trim) {
        Some("signfail") if !configured => row(
            Status::Warn,
            "signing is not set up here",
            Some(
                "git's config there sets neither user.signingkey nor commit.gpgsign; set them if tasks should commit signed"
                    .into(),
            ),
        ),
        Some("signed") => match lines.next().map(str::trim) {
            Some("verified") => row(Status::Pass, "a commit here signs and verifies", None),
            Some("verifyfail") => row(
                Status::Fail,
                "a signed commit here does not verify",
                git_words(&lines.collect::<Vec<_>>().join("\n")),
            ),
            _ => row(
                Status::Error,
                "could not verify a signed commit",
                Some(format!("the answer ended early ({})", run::exit(ran.code))),
            ),
        },
        Some("signfail") => row(
            Status::Fail,
            "a commit here does not sign",
            git_words(&lines.collect::<Vec<_>>().join("\n")),
        ),
        _ => row(
            Status::Error,
            "could not try a signed commit",
            Some(format!(
                "the machine's answer was not understood ({}){}",
                run::exit(ran.code),
                run::stderr_tail(&ran)
            )),
        ),
    }
}

/// Check `m`, reached through `shell`, with its provisioner (`want` names
/// one instead). An error is a run that gave no rows: no shell on the
/// machine, an unknown `want`, or setting up the run failed.
#[allow(clippy::too_many_arguments)]
pub async fn check(
    shell: Arc<dyn Shell>,
    m: &MachineConfig,
    flock: &Flock,
    config: &Config<'_>,
    catalog: &Catalog,
    want: Option<&str>,
    herdr_running: Option<&str>,
    herdr: Option<Arc<dyn crate::herdr::transport::Connector>>,
) -> anyhow::Result<Report> {
    let (p, picked_because) = match pick(&shell, m, catalog, want).await? {
        Ok(picked) => picked,
        Err(tried) => {
            return Ok(Report {
                machine: m.name.clone(),
                provisioner: None,
                version: None,
                agents: vec![],
                rows: vec![Row {
                    id: PROVISIONER_NONE_FITS.into(),
                    agent: None,
                    status: Status::Fail,
                    first_line: String::new(),
                    reason: Some(format!(
                        "no provisioner fits {}: {}; name one with --provisioner or `provisioner` in flock.toml",
                        m.name,
                        tried.join("; ")
                    )),
                }],
            });
        }
    };
    let agent_envs = agents_on(m, flock, config);
    let agents: Vec<String> = agent_envs.iter().map(|a| a.name.clone()).collect();
    let accounts = accounts_of(m, flock);
    let req = run::Request {
        machine: &m.name,
        provisioner: p,
        accounts: &accounts,
        agents: &agents,
        env: &BTreeMap::new(),
        agent_envs: &agent_envs,
    };
    let mut rows = run::run(shell.clone(), &req)
        .await
        .map_err(|e| CliError::err(e.code(), e))?;
    // The machine's own profile is what its tasks run under unless they ask
    // for another: an account rule that shadows its allows stalls them.
    let own = config
        .defaults
        .own_profile(Some(m), flock.entry(flock.primary_flock(m)))
        .and_then(|name| config.profiles.resolve(&name).ok());
    rows.extend(profile_rows(m, flock, config));
    if let Some(profile) = own {
        rows.extend(account_rules::rows(shell.as_ref(), &profile, &agent_envs).await);
    }
    if agent_envs
        .iter()
        .any(|a| a.kind == crate::config::opencode::KIND)
    {
        let profiled = picks_on(m, flock, config).iter().any(|(_, _, pick)| {
            pick.profile.is_some()
                && config.agents.kind(&pick.agent) == crate::config::opencode::KIND
                && !config.agents.own_permissions(&pick.agent)
        });
        rows.push(opencode_row(shell.as_ref(), profiled).await);
    }
    rows.extend(
        harness_rows(
            &m.name,
            shell.as_ref(),
            config.harnesses,
            &flock.pins_for(m),
            &agent_envs,
        )
        .await,
    );
    rows.push(path_row(shell.as_ref(), config.harnesses, &agent_envs).await);
    if let (Some(head), Some(_)) = (config.head_address, &m.ssh) {
        rows.push(reach_row(shell.as_ref(), &m.name, head).await);
    }
    if accounts.contains(&Account::Github) {
        let expected = expected_names(m, flock, Account::Github);
        rows.extend(gh_auth_rows(shell.as_ref(), &m.name, chrono::Utc::now(), &expected).await);
    }
    let expected = expected_names(m, flock, Account::Claude);
    if !expected.is_empty() {
        rows.extend(
            claude_account_rows(shell.as_ref(), &m.name, &agent_envs, &rows, &expected).await,
        );
    }
    rows.push(signing_row(shell.clone(), &m.name, SIGNING_TIMEOUT).await);
    // Only with a herdr that answered: one that did not has its own row.
    if let (Some(conn), Some(_)) = (herdr, herdr_running) {
        let layers = crate::config::EnvLayers::of(config.defaults, flock, m)
            .merged(Some(flock.primary_flock(m)));
        let head = config.head_address.filter(|_| m.ssh.is_some());
        let wants = pane_env::wants(&layers, &agent_envs, head);
        rows.extend(pane_env::rows(shell.as_ref(), conn, &m.name, wants, pane_env::TIMEOUT).await);
    }
    name_login_commands(&m.name, config.harnesses, &mut rows);
    if config.herdr_pin.is_some() || herdr_running.is_some() {
        let installed = installed_herdr(shell.as_ref()).await;
        if let Some(pin) = config.herdr_pin {
            rows.push(herdr_row(&m.name, &installed, pin));
        }
        rows.extend(herdr_running.and_then(|running| herdr_running_row(&installed, running)));
    }
    if let Some(because) = picked_because {
        rows.insert(
            0,
            Row {
                id: PROVISIONER_PICKED.into(),
                agent: None,
                status: Status::Pass,
                first_line: because,
                reason: None,
            },
        );
    }
    Ok(Report {
        machine: m.name.clone(),
        provisioner: Some(p.id.clone()),
        version: Some(p.manifest.version.to_string()),
        agents,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PastorConfig;
    use crate::provision::load_manifest;
    use crate::provision::run::testing::Fake;
    use std::path::Path;

    /// A machine's home, the provisioners copied beside it, and what
    /// pastor.toml and flock.toml say.
    struct Setup {
        _tmp: tempfile::TempDir,
        root: std::path::PathBuf,
        fake: Arc<Fake>,
        provisioners: Vec<Provisioner>,
        config: PastorConfig,
        flock: Flock,
        /// `[artifacts] herdr`.
        herdr_pin: Option<String>,
        /// The version of the herdr server running there, if it answered.
        herdr_running: Option<String>,
        /// `head_address` of pastor.toml.
        head_address: Option<String>,
    }

    const PASTOR_TOML: &str = "[agents.claude-main]\nkind = \"claude\"\n\
         env = { CLAUDE_CONFIG_DIR = \"~/.claude-main\" }\n";

    /// The OS here, as `uname -s` says it, and one it is not.
    fn here() -> &'static str {
        crate::provision::run::testing::os_here()
    }
    fn elsewhere() -> &'static str {
        if here() == "linux" { "darwin" } else { "linux" }
    }

    fn setup(flock: &str) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        // opencode's managed config and preferences in dirs of the test's
        // own, empty unless a test fills them, not this host's.
        let mut fake = Fake::new(&home, None);
        fake.vars = vec![
            (
                "OPENCODE_TEST_MANAGED_CONFIG_DIR".into(),
                root.join("managed").to_string_lossy().into(),
            ),
            (
                "PASTOR_TEST_MANAGED_PREFERENCES_DIR".into(),
                root.join("preferences").to_string_lossy().into(),
            ),
        ];
        Setup {
            fake: Arc::new(fake),
            _tmp: tmp,
            root,
            provisioners: vec![],
            config: PastorConfig::parse(Path::new("pastor.toml"), PASTOR_TOML).unwrap(),
            flock: Flock::parse(Path::new("flock.toml"), flock).unwrap(),
            herdr_pin: None,
            herdr_running: None,
            head_address: None,
        }
    }

    impl Setup {
        /// A copied provisioner `id` for `platform` with these checks.
        fn provisioner(&mut self, id: &str, platform: &str, checks: &str) {
            let dir = self.root.join("prov").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(crate::provision::MANIFEST_FILE),
                format!("id = \"{id}\"\nversion = \"0.2.0\"\n[platform]\n{platform}\n{checks}"),
            )
            .unwrap();
            let manifest = load_manifest(&dir, Some(id)).unwrap();
            self.provisioners.push(Provisioner {
                id: id.into(),
                dir: Some(dir),
                manifest,
            });
        }

        async fn check(&self, machine: &str, want: Option<&str>) -> anyhow::Result<Report> {
            let catalog = Catalog::of(self.provisioners.clone());
            self.check_on(self.fake.clone(), &catalog, machine, want)
                .await
        }

        /// `machine check`, through `shell`, with `catalog`, without the
        /// `machine check`, through `shell`, with `catalog`, without the `path`,
        /// `gh-auth` and `signing` rows: they say what this host's PATH holds,
        /// how its gh is logged in and whether a commit signs with its git,
        /// which most tests are not about (`check_raw_on` keeps them).
        async fn check_on(
            &self,
            shell: Arc<dyn Shell>,
            catalog: &Catalog,
            machine: &str,
            want: Option<&str>,
        ) -> anyhow::Result<Report> {
            let mut report = self.check_raw_on(shell, catalog, machine, want).await?;
            report
                .rows
                .retain(|r| r.id != PATH_ROW && r.id != GH_AUTH_ROW && r.id != SIGNING_ROW);
            Ok(report)
        }

        /// `check_on`, with every row.
        async fn check_raw_on(
            &self,
            shell: Arc<dyn Shell>,
            catalog: &Catalog,
            machine: &str,
            want: Option<&str>,
        ) -> anyhow::Result<Report> {
            let m = self.flock.get(machine).unwrap();
            let config = Config {
                defaults: &self.config.defaults,
                agents: &self.config.agents,
                models: &self.config.models,
                profiles: &self.config.profiles,
                harnesses: crate::harness_manifest::Harnesses::builtin(),
                herdr_pin: self.herdr_pin.as_deref(),
                head_address: self.head_address.as_deref(),
            };
            check(
                shell,
                m,
                &self.flock,
                &config,
                catalog,
                want,
                self.herdr_running.as_deref(),
                None,
            )
            .await
        }
    }

    /// A machine that is not this one, to the probes: `uname` on its PATH
    /// says `uname`, and the os-release (given as its text) and device tree
    /// model the probes and checks read are files under `root` (none when
    /// not given).
    struct Fixture {
        fake: Fake,
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn shell(
            root: &Path,
            uname: &str,
            release: Option<&str>,
            model: Option<&str>,
        ) -> Arc<dyn Shell> {
            let bin = root.join("bin");
            let home = root.join("home");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            let uname_bin = bin.join("uname");
            std::fs::write(&uname_bin, format!("#!/bin/sh\necho {uname}\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&uname_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            if let Some(release) = release {
                std::fs::create_dir_all(root.join("etc")).unwrap();
                std::fs::write(root.join("etc/os-release"), release).unwrap();
            }
            if let Some(model) = model {
                std::fs::create_dir_all(root.join("proc/device-tree")).unwrap();
                std::fs::write(root.join("proc/device-tree/model"), format!("{model}\0")).unwrap();
            }
            let path = format!("{}:/usr/bin:/bin", bin.display());
            Arc::new(Fixture {
                fake: Fake::new(&home, Some(path)),
                root: root.to_path_buf(),
            })
        }
    }

    impl Shell for Fixture {
        fn run(
            &self,
            script: String,
            input: Vec<u8>,
            time: std::time::Duration,
        ) -> run::ShellFuture<'_> {
            // A check's script comes on the stdin of `CHECK_COMMAND`; any
            // other input is the provisioner's tarball, left as it is.
            let at_root = |text: &str| {
                let mut text = text.to_string();
                for file in ["/etc/os-release", "/proc/device-tree/model"] {
                    text = text.replace(file, &format!("{}{file}", self.root.display()));
                }
                text
            };
            let input = match std::str::from_utf8(&input) {
                Ok(text) if script == run::CHECK_COMMAND => at_root(text).into_bytes(),
                _ => input,
            };
            self.fake.run(at_root(&script), input, time)
        }
    }

    fn check_toml(id: &str, script: &str, extra: &str) -> String {
        format!(
            "[[check]]\nid = \"{id}\"\ncommand = [\"sh\", \"-c\", {}]\n{extra}\n",
            toml::Value::String(script.into())
        )
    }

    const ONE_MACHINE: &str = "[[flock]]\nname = \"work\"\ndefault = true\n\
         machines = { pi-3 = 2 }\n[[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n";

    fn ids(report: &Report) -> Vec<(String, Option<String>, Status)> {
        report
            .rows
            .iter()
            .map(|r| (r.id.clone(), r.agent.clone(), r.status))
            .collect()
    }

    /// A machine that passes every check is ok, and its rows say which
    /// provisioner ran.
    #[tokio::test]
    async fn a_passing_machine_is_ok() {
        let mut s = setup(ONE_MACHINE);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "echo git here", ""));
        let report = s.check("pi-3", None).await.unwrap();
        assert!(report.ok(), "{report:?}");
        assert_eq!(report.provisioner.as_deref(), Some("mine"));
        assert_eq!(report.version.as_deref(), Some("0.2.0"));
        assert_eq!(
            report.text(),
            "pi-3: provisioner mine 0.2.0\nok     git  git here\n"
        );
    }

    /// With a profile for the machine, each Claude account's settings are
    /// read: an ask rule over one of the profile's allows is a `warn` row
    /// naming both, which does not make the machine fail. A machine with no
    /// profile gets no such row.
    #[tokio::test]
    async fn an_account_rule_over_the_profiles_allow_warns_without_failing() {
        let mut s = setup(ONE_MACHINE);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let settings = s.root.join("home/.claude/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, r#"{"permissions":{"ask":["Bash(git push:*)"]}}"#).unwrap();

        let report = s.check("pi-3", None).await.unwrap();
        assert_eq!(
            ids(&report).len(),
            1,
            "no profile, no account row: {report:?}"
        );

        s.config.defaults.profile = Some("develop".into());
        let report = s.check("pi-3", None).await.unwrap();
        assert!(report.ok(), "a warning is not a failure: {report:?}");
        assert_eq!(report.failed().count(), 0);
        assert_eq!(
            ids(&report),
            [
                ("git".to_string(), None, Status::Pass),
                (
                    PROFILE_ROW.to_string(),
                    Some("claude".to_string()),
                    Status::Pass
                ),
                (
                    "account-rules".to_string(),
                    Some("claude".to_string()),
                    Status::Warn
                ),
            ]
        );
        assert!(
            report.text().contains(
                "warn   account-rules (claude)  ask Bash(git push:*) in ~/.claude/settings.json shadows profile develop's allow Bash\n"
            ),
            "{}",
            report.text()
        );

        std::fs::write(&settings, "{}").unwrap();
        let report = s.check("pi-3", None).await.unwrap();
        assert_eq!(report.rows[2].status, Status::Pass, "{report:?}");
    }

    /// One failed check makes the machine not ok and is named.
    #[tokio::test]
    async fn a_failed_check_is_named() {
        let mut s = setup(ONE_MACHINE);
        let os = format!("os = \"{}\"", here());
        let checks = check_toml("git", "true", "")
            + &check_toml("gh", "echo gh is missing; exit 1", "")
            + &check_toml("gh-login", "true", "needs = [\"gh\"]");
        s.provisioner("mine", &os, &checks);
        let report = s.check("pi-3", None).await.unwrap();
        assert!(!report.ok());
        let failed: Vec<&str> = report.failed().map(|r| r.id.as_str()).collect();
        assert_eq!(failed, ["gh"]);
        let text = report.text();
        assert!(text.contains("\nfail   gh  gh is missing\n"), "{text}");
        assert!(
            text.contains("\nskip   gh-login  needs gh, which did not pass\n"),
            "{text}"
        );
    }

    /// `--provisioner` beats the machine's own `provisioner`.
    #[tokio::test]
    async fn the_asked_provisioner_wins() {
        let mut s = setup(&ONE_MACHINE.replace(
            "ssh = \"user@pi-3\"\n",
            "ssh = \"user@pi-3\"\nprovisioner = \"named\"\n",
        ));
        let os = format!("os = \"{}\"", here());
        s.provisioner("asked", &os, &check_toml("a", "true", ""));
        s.provisioner("named", &os, &check_toml("n", "true", ""));
        let named = s.check("pi-3", None).await.unwrap();
        assert_eq!(named.provisioner.as_deref(), Some("named"));
        let asked = s.check("pi-3", Some("asked")).await.unwrap();
        assert_eq!(asked.provisioner.as_deref(), Some("asked"));
        assert_eq!(asked.rows[0].id, "a");
        let err = s.check("pi-3", Some("nope")).await.unwrap_err();
        let err = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(err.code, UNKNOWN_PROVISIONER);
    }

    /// With no provisioner named, the first that fits the machine's OS,
    /// distribution and `detect` is the one; none fitting is a failing
    /// row that says what was tried.
    #[tokio::test]
    async fn the_first_fit_is_picked_and_none_fitting_is_a_failed_row() {
        let mut s = setup(ONE_MACHINE);
        let os = format!("os = \"{}\"", here());
        s.provisioner(
            "a-other-os",
            &format!("os = \"{}\"", elsewhere()),
            &check_toml("x", "true", ""),
        );
        s.provisioner(
            "b-no-detect",
            &format!("{os}\ndetect = [\"sh\", \"-c\", \"echo not this one; exit 1\"]"),
            &check_toml("x", "true", ""),
        );
        let report = s.check("pi-3", None).await.unwrap();
        assert_eq!(report.provisioner, None);
        assert_eq!(
            ids(&report),
            [(PROVISIONER_NONE_FITS.to_string(), None, Status::Fail)]
        );
        assert!(!report.ok());
        let why = report.rows[0].reason.clone().unwrap();
        assert!(
            why.contains(&format!(
                "a-other-os is for {}, not {}",
                elsewhere(),
                here()
            )),
            "{why}"
        );
        assert!(
            why.contains("b-no-detect's detect gave exit 1 (not this one)"),
            "{why}"
        );

        s.provisioner(
            "c-fits",
            &format!("{os}\ndetect = [\"true\"]"),
            &check_toml("y", "true", ""),
        );
        let report = s.check("pi-3", None).await.unwrap();
        assert_eq!(report.provisioner.as_deref(), Some("c-fits"));
    }

    /// With no provisioner named, the built-ins split machines by what
    /// they say: Arch, Debian, a Pi on Raspberry Pi OS (debian on the
    /// 64-bit image, raspbian on the 32-bit one) and a Mac each get theirs,
    /// a machine that is only like one of them (`ID_LIKE`: Ubuntu, Mint,
    /// EndeavourOS) gets that one, with a row that says why, and another
    /// Linux gets none.
    #[tokio::test]
    async fn the_built_ins_fit_the_machines_they_are_for() {
        let s = setup(ONE_MACHINE);
        let catalog = Catalog::builtin();
        let pi5 = "Raspberry Pi 5 Model B Rev 1.0";
        let ubuntu = "ID=ubuntu\nID_LIKE=debian\n";
        let cases = [
            ("Linux", Some("ID=arch\n"), None, Some("arch")),
            ("Linux", Some("ID=debian\n"), None, Some("debian")),
            (
                "Linux",
                Some("ID=debian\n"),
                Some("QEMU Virtual Machine"),
                Some("debian"),
            ),
            (
                "Linux",
                Some("ID=debian\n"),
                Some(pi5),
                Some("raspberry-pi-os"),
            ),
            (
                "Linux",
                Some("ID=raspbian\n"),
                Some("Raspberry Pi 3 Model B Rev 1.2"),
                Some("raspberry-pi-os"),
            ),
            ("Darwin", None, None, Some("macos")),
            ("Linux", Some("ID=fedora\n"), None, None),
            // Ubuntu is named by the debian built-in, and Derivatives
            // through ID_LIKE, the quotes os-release allows.
            ("Linux", Some(ubuntu), None, Some("debian")),
            ("Linux", Some(ubuntu), Some(pi5), Some("raspberry-pi-os")),
            (
                "Linux",
                Some("ID=elementary\nID_LIKE=ubuntu\n"),
                None,
                Some("debian"),
            ),
            ("Linux", Some("ID_LIKE=debian\n"), None, Some("debian")),
            (
                "Linux",
                Some("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"),
                None,
                Some("debian"),
            ),
            (
                "Linux",
                Some("ID=endeavouros\nID_LIKE=arch\n"),
                None,
                Some("arch"),
            ),
            // Like nothing a built-in is for.
            (
                "Linux",
                Some("ID=rocky\nID_LIKE=\"rhel centos fedora\"\n"),
                None,
                None,
            ),
            ("Linux", Some("NAME=Odd\n"), None, None),
        ];
        for (i, (uname, release, model, want)) in cases.into_iter().enumerate() {
            let shell = Fixture::shell(&s.root.join(format!("m{i}")), uname, release, model);
            let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
            assert_eq!(
                report.provisioner.as_deref(),
                want,
                "{uname} {release:?} {model:?}: {report:?}"
            );
            if want.is_none() {
                assert_eq!(report.rows[0].id, PROVISIONER_NONE_FITS);
            }
            // The row that says why is there for a like match, and only then:
            // not for Ubuntu itself, which the debian built-in names.
            let like = release.is_some_and(|r| r.contains("ID_LIKE"))
                && want.is_some()
                && !(release == Some(ubuntu) && want == Some("debian"));
            let said = report.rows.iter().find(|r| r.id == PROVISIONER_PICKED);
            assert_eq!(said.is_some(), like, "{release:?}: {report:?}");
            if let Some(row) = said {
                assert_eq!(row.status, Status::Pass);
                assert!(
                    row.first_line
                        .starts_with(&format!("{}, because os-release ", want.unwrap())),
                    "{}",
                    row.first_line
                );
                // Without an ID it does not call the machine "this".
                if !release.unwrap().contains("\nID=") && !release.unwrap().starts_with("ID=") {
                    assert_eq!(
                        row.first_line,
                        "debian, because os-release has no ID and has ID_LIKE debian"
                    );
                }
                assert_eq!(report.rows[0].id, PROVISIONER_PICKED, "first");
            }
        }
    }

    /// What the none-fitting row says includes the `ID_LIKE` it tried.
    #[tokio::test]
    async fn the_none_fitting_row_names_what_the_machine_is_like() {
        let s = setup(ONE_MACHINE);
        let shell = Fixture::shell(
            &s.root.join("m"),
            "Linux",
            Some("ID=rocky\nID_LIKE=\"rhel fedora\"\n"),
            None,
        );
        let report = s
            .check_on(shell, &Catalog::builtin(), "pi-3", None)
            .await
            .unwrap();
        let why = report.rows[0].reason.clone().unwrap();
        assert!(
            why.contains("arch is for arch, not rocky (ID_LIKE rhel fedora)"),
            "{why}"
        );
    }

    /// The words of `ID_LIKE` are tried in their order, nearest first, not in
    /// the catalog's: Mint says `ubuntu debian`, so a provisioner for
    /// `ubuntu` wins over one for `debian` that comes before it by id.
    #[tokio::test]
    async fn id_like_is_tried_in_its_order() {
        let mut s = setup(ONE_MACHINE);
        for (id, distro) in [("a-debian", "debian"), ("b-ubuntu", "ubuntu")] {
            s.provisioner(
                id,
                &format!("os = \"linux\"\ndistro = [\"{distro}\"]"),
                &check_toml("x", "true", ""),
            );
        }
        let catalog = Catalog::of(s.provisioners.clone());
        let shell = Fixture::shell(
            &s.root.join("m"),
            "Linux",
            Some("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"),
            None,
        );
        let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
        assert_eq!(report.provisioner.as_deref(), Some("b-ubuntu"));
        assert_eq!(
            report.rows[0].first_line,
            "b-ubuntu, because os-release ID linuxmint has ID_LIKE ubuntu"
        );
        // The other order of the words, the other pick.
        let shell = Fixture::shell(
            &s.root.join("n"),
            "Linux",
            Some("ID=other\nID_LIKE=\"debian ubuntu\"\n"),
            None,
        );
        let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
        assert_eq!(report.provisioner.as_deref(), Some("a-debian"));
    }

    /// A provisioner for the machine's own `ID` beats one for a distribution
    /// it is like: Ubuntu gets an `ubuntu` one, not Debian's.
    #[tokio::test]
    async fn the_machines_own_id_beats_id_like() {
        let mut s = setup(ONE_MACHINE);
        for (id, distro) in [("a-debian", "debian"), ("b-ubuntu", "ubuntu")] {
            s.provisioner(
                id,
                &format!("os = \"linux\"\ndistro = [\"{distro}\"]"),
                &check_toml("x", "true", ""),
            );
        }
        let catalog = Catalog::of(s.provisioners.clone());
        let shell = Fixture::shell(
            &s.root.join("m"),
            "Linux",
            Some("ID=ubuntu\nID_LIKE=debian\n"),
            None,
        );
        let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
        assert_eq!(report.provisioner.as_deref(), Some("b-ubuntu"));
        assert!(report.rows.iter().all(|r| r.id != PROVISIONER_PICKED));
    }

    /// A fake CLI that prints `says` for `--version`, in the machine's
    /// `~/.local/bin`, and a shell whose PATH has no other.
    fn machine_with_cli(root: &Path, name: &str, says: Option<&str>) -> Arc<dyn Shell> {
        use std::os::unix::fs::PermissionsExt;
        let home = root.join("home");
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        if let Some(says) = says {
            let cli = bin.join(name);
            std::fs::write(&cli, format!("#!/bin/sh\necho '{says}'\n")).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Arc::new(Fake::new(&home, Some("/usr/bin:/bin".into())))
    }

    /// A machine with these commands (as `name -> directory`, made under its
    /// home) and a PATH of these directories before the system's.
    fn machine_with_bins(root: &Path, bins: &[(&str, &str)], path: &[&str]) -> Arc<dyn Shell> {
        use std::os::unix::fs::PermissionsExt;
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        for (name, dir) in bins {
            let dir = home.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join(name);
            std::fs::write(&f, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path: Vec<String> = path
            .iter()
            .map(|d| home.join(d).display().to_string())
            .chain(["/usr/bin".into(), "/bin".into()])
            .collect();
        Arc::new(Fake::new(&home, Some(path.join(":"))))
    }

    /// The `path` row says the directories pastor's commands see, and
    /// whether herdr and the CLI of each harness the machine's agents run
    /// resolve: found on the PATH; found only in a directory pastor adds
    /// for its own checks (`~/.local/bin`), which it says; or not found,
    /// which warns, naming it. It never fails the check.
    #[tokio::test]
    async fn the_path_row_says_what_resolves() {
        let mut s = setup(PINNED);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        let row = |report: &Report| -> Row {
            report
                .rows
                .iter()
                .find(|r| r.id == PATH_ROW)
                .unwrap_or_else(|| panic!("no path row: {report:?}"))
                .clone()
        };
        let both = [("herdr", "bin"), ("claude", "bin")];

        // On the PATH.
        let shell = machine_with_bins(&s.root.join("a"), &both, &["bin"]);
        let report = s.check_raw_on(shell, &catalog, "pi-3", None).await.unwrap();
        let r = row(&report);
        assert_eq!(r.status, Status::Pass, "{r:?}");
        assert!(
            r.first_line.starts_with("herdr, claude found; PATH "),
            "{r:?}"
        );
        assert!(r.first_line.contains("/home/bin:/usr/bin:/bin"), "{r:?}");
        assert!(!r.first_line.contains("pastor adds"), "{r:?}");

        // Only in ~/.local/bin, which a non-login shell's PATH lacks.
        let both_local = [("herdr", ".local/bin"), ("claude", ".local/bin")];
        let shell = machine_with_bins(&s.root.join("b"), &both_local, &[]);
        let report = s.check_raw_on(shell, &catalog, "pi-3", None).await.unwrap();
        let r = row(&report);
        assert_eq!(r.status, Status::Pass, "{r:?}");
        assert!(
            r.first_line.contains("claude at ") && r.first_line.contains("a directory pastor adds"),
            "{r:?}"
        );
        assert!(r.first_line.ends_with("PATH /usr/bin:/bin"), "{r:?}");

        // A harness CLI nowhere: a warning that names it, the check still ok.
        let shell = machine_with_bins(&s.root.join("c"), &[("herdr", "bin")], &["bin"]);
        let report = s.check_raw_on(shell, &catalog, "pi-3", None).await.unwrap();
        let r = row(&report);
        assert_eq!(r.status, Status::Warn, "{r:?}");
        assert!(r.first_line.starts_with("claude not found; PATH "), "{r:?}");
        assert!(r.reason.as_deref().unwrap().contains("install it"), "{r:?}");
        assert!(
            report.failed().all(|r| r.id != PATH_ROW),
            "a warning does not fail the check"
        );
        assert!(
            report.text().contains("warn   path  claude not found"),
            "{}",
            report.text()
        );

        // herdr nowhere.
        let shell = machine_with_bins(&s.root.join("d"), &[("claude", "bin")], &["bin"]);
        let report = s.check_raw_on(shell, &catalog, "pi-3", None).await.unwrap();
        assert!(
            row(&report).first_line.starts_with("herdr not found"),
            "{:?}",
            row(&report)
        );
    }

    /// With `[artifacts] herdr` set, a `herdr-version` row: `ok` at the pin;
    /// `warn` naming the fix at another version or with no herdr; none when
    /// nothing is pinned. A drift does not fail the check.
    #[tokio::test]
    async fn herdr_at_another_version_than_its_pin_warns() {
        let mut s = setup(PINNED);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        let herdr_rows = |report: &Report| -> Vec<(Status, String, Option<String>)> {
            report
                .rows
                .iter()
                .filter(|r| r.id == HERDR_VERSION_ROW)
                .map(|r| (r.status, r.first_line.clone(), r.reason.clone()))
                .collect()
        };
        let fix = Some("fix with `pastor machine install pi-3 --kind herdr`".to_string());
        for (n, says, want) in [
            (
                0,
                Some("herdr 0.9.5"),
                vec![(Status::Pass, "herdr 0.9.5, pinned 0.9.5".to_string(), None)],
            ),
            (
                1,
                Some("herdr 0.9.4"),
                vec![(
                    Status::Warn,
                    "herdr 0.9.4, pinned 0.9.5".to_string(),
                    fix.clone(),
                )],
            ),
            (
                2,
                None,
                vec![(
                    Status::Warn,
                    "herdr is not on the machine's PATH, pinned 0.9.5".to_string(),
                    fix.clone(),
                )],
            ),
        ] {
            s.herdr_pin = Some("0.9.5".into());
            let shell = machine_with_cli(&s.root.join(format!("h{n}")), "herdr", says);
            let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
            assert_eq!(herdr_rows(&report), want, "{says:?}: {report:?}");
            assert!(report.ok(), "a drift is a warning: {report:?}");
        }
        // Nothing pinned, nothing asked.
        s.herdr_pin = None;
        let shell = machine_with_cli(&s.root.join("none"), "herdr", Some("herdr 0.9.4"));
        let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
        assert!(herdr_rows(&report).is_empty(), "{report:?}");
    }

    /// A herdr server running at another version than the one now installed
    /// is a `herdr-running` warning saying it keeps its version until it is
    /// restarted; the same version, no herdr installed or a server that did
    /// not answer is none. It needs no pin.
    #[tokio::test]
    async fn a_running_herdr_that_is_not_the_installed_one_warns() {
        let mut s = setup(PINNED);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        for (n, installed, running, want) in [
            (
                0,
                Some("herdr 0.9.5"),
                Some("0.9.4"),
                vec![(
                    Status::Warn,
                    "herdr 0.9.5 is installed, herdr 0.9.4 is running",
                )],
            ),
            (1, Some("herdr 0.9.5"), Some("0.9.5"), vec![]),
            (2, None, Some("0.9.4"), vec![]),
            (3, Some("herdr 0.9.5"), None, vec![]),
        ] {
            s.herdr_running = running.map(str::to_string);
            let shell = machine_with_cli(&s.root.join(format!("r{n}")), "herdr", installed);
            let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
            let got: Vec<(Status, &str)> = report
                .rows
                .iter()
                .filter(|r| r.id == HERDR_RUNNING_ROW)
                .map(|r| (r.status, r.first_line.as_str()))
                .collect();
            assert_eq!(got, want, "{installed:?} {running:?}: {report:?}");
            assert!(report.ok(), "a warning does not fail the check");
            if let Some(row) = report.rows.iter().find(|r| r.id == HERDR_RUNNING_ROW) {
                assert!(
                    row.reason
                        .as_deref()
                        .unwrap()
                        .contains("until it is restarted"),
                    "{row:?}"
                );
            }
            assert!(
                report.rows.iter().all(|r| r.id != HERDR_VERSION_ROW),
                "no pin, no version row"
            );
        }
    }

    const PINNED: &str = "[[flock]]\nname = \"work\"\ndefault = true\nmachines = { pi-3 = 2 }\n\
         pins = { claude = \"2.1.310\" }\n\
         [[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n";

    /// A machine whose CLI is at the pin passes, one at another version gets a
    /// `warn` row naming both, which does not fail the check; no row for a CLI
    /// that is not there, an unpinned kind, `latest`, or a machine's own pin
    /// that says `latest`.
    #[tokio::test]
    async fn a_harness_at_another_version_than_its_pin_warns() {
        let mut s = setup(PINNED);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        let row = |report: &Report| -> Vec<(String, Option<String>, Status, String)> {
            report
                .rows
                .iter()
                .filter(|r| r.id == harness::VERSION_ROW)
                .map(|r| {
                    (
                        r.id.clone(),
                        r.agent.clone(),
                        r.status,
                        r.first_line.clone(),
                    )
                })
                .collect()
        };
        for (says, want) in [
            (
                Some("2.1.310 (Claude Code)"),
                vec![(Status::Pass, "claude 2.1.310, pinned 2.1.310")],
            ),
            (
                Some("2.1.309 (Claude Code)"),
                vec![(Status::Warn, "claude 2.1.309, pinned 2.1.310")],
            ),
            (None, vec![]),
        ] {
            let shell = machine_with_cli(
                &s.root
                    .join(format!("{says:?}").replace(['"', ' ', '(', ')'], "")),
                "claude",
                says,
            );
            let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
            let got: Vec<(Status, String)> = row(&report).into_iter().map(|r| (r.2, r.3)).collect();
            let want: Vec<(Status, String)> = want
                .into_iter()
                .map(|(st, t)| (st, t.to_string()))
                .collect();
            assert_eq!(got, want, "{says:?}: {report:?}");
            assert!(report.ok(), "a drift is a warning: {report:?}");
            assert_eq!(report.failed().count(), 0);
        }
        // The row is the agent's, and it reads right in the text.
        let shell = machine_with_cli(&s.root.join("w"), "claude", Some("2.1.309 (Claude Code)"));
        let report = s.check_on(shell, &catalog, "pi-3", None).await.unwrap();
        assert!(
            report
                .text()
                .contains("warn   harness-version (claude)  claude 2.1.309, pinned 2.1.310: fix with `pastor machine install pi-3 --kind claude`\n"),
            "{}",
            report.text()
        );
        // The machine's own pin replaces its flock's, per kind.
        let mut over = setup(&PINNED.replace(
            "ssh = \"user@pi-3\"\n",
            "ssh = \"user@pi-3\"\npins = { claude = \"2.1.309\" }\n",
        ));
        over.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(over.provisioners.clone());
        let shell = machine_with_cli(
            &over.root.join("w"),
            "claude",
            Some("2.1.309 (Claude Code)"),
        );
        let report = over.check_on(shell, &catalog, "pi-3", None).await.unwrap();
        assert_eq!(row(&report)[0].2, Status::Pass, "{report:?}");
        // `latest` pins nothing: no row at all.
        let mut any = setup(&PINNED.replace("2.1.310", "latest"));
        any.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(any.provisioners.clone());
        let shell = machine_with_cli(&any.root.join("w"), "claude", Some("0.0.1"));
        let report = any.check_on(shell, &catalog, "pi-3", None).await.unwrap();
        assert!(row(&report).is_empty(), "{report:?}");
    }

    /// The version row asks the way the harness's manifest says, so a
    /// harness only a user manifest describes is checked against its pin
    /// like claude is: its binary, its version arguments, its install dirs.
    #[tokio::test]
    async fn a_version_row_follows_the_manifest() {
        let root = tempfile::tempdir().unwrap();
        let manifest = crate::harness_manifest::Manifest::parse(
            "id = \"tool\"\nbinary = \"tool\"\nbin_dirs = [\"~/.local/bin\"]\n\
             [version]\nargs = [\"-V\"]\n[paths]\nhome = \"~/.tool\"\n",
        )
        .unwrap();
        let harnesses = crate::harness_manifest::Harnesses::builtin()
            .clone()
            .with(manifest);
        let shell = machine_with_cli(root.path(), "tool", Some("tool version 1.2.9"));
        let agents = [AgentEnv {
            name: "tool-1".into(),
            kind: "tool".into(),
            env: BTreeMap::new(),
        }];
        let rows = |pin: &str| {
            let pins: harness::Pins = [("tool".to_string(), pin.to_string())].into();
            let (shell, harnesses, agents) = (shell.clone(), harnesses.clone(), agents.clone());
            async move { harness_rows("tool-box", shell.as_ref(), &harnesses, &pins, &agents).await }
        };
        let got = rows("1.2").await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(
            (got[0].status, got[0].first_line.as_str()),
            (Status::Pass, "tool 1.2.9, pinned 1.2")
        );
        let got = rows("1.3").await;
        assert_eq!(got[0].status, Status::Warn, "{got:?}");
        // A kind whose manifest reads no version has nothing to ask: a
        // warning says so.
        let pins: harness::Pins = [("codex".to_string(), "1".to_string())].into();
        let codex = [AgentEnv {
            name: "codex".into(),
            kind: "codex".into(),
            env: BTreeMap::new(),
        }];
        let got = harness_rows("tool-box", shell.as_ref(), &harnesses, &pins, &codex).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].status, Status::Warn);
    }

    /// A pin on a harness whose manifest reads no version is a warning that
    /// says so, not silence.
    #[tokio::test]
    async fn a_pin_on_a_manifest_with_no_version_warns() {
        let root = tempfile::tempdir().unwrap();
        let manifest = crate::harness_manifest::Manifest::parse(
            "id = \"claude\"\n[paths]\nhome = \"~/.claude\"\n",
        )
        .unwrap();
        let harnesses = crate::harness_manifest::Harnesses::builtin()
            .clone()
            .with(manifest);
        let shell = machine_with_cli(root.path(), "claude", Some("2.1.310"));
        let agents = [AgentEnv {
            name: "claude".into(),
            kind: "claude".into(),
            env: BTreeMap::new(),
        }];
        let pins: harness::Pins = [("claude".to_string(), "2.1".to_string())].into();
        let got = harness_rows("tool-box", shell.as_ref(), &harnesses, &pins, &agents).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].status, Status::Warn);
        assert!(
            got[0].first_line.contains("manifest reads no version"),
            "{got:?}"
        );
    }

    /// Every Claude agent a task can get on the machine gets its own login
    /// row, with its own config dir; a kind no agent there is gets none.
    #[tokio::test]
    async fn two_claude_agents_get_two_login_rows() {
        let flock = "[[flock]]\nname = \"home\"\ndefault = true\nmachines = { pi-3 = 1 }\n\
             [[flock]]\nname = \"work\"\nagent = \"claude-main\"\nmachines = { pi-3 = 1 }\n\
             [[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n";
        let mut s = setup(flock);
        let os = format!("os = \"{}\"", here());
        let checks = check_toml(
            "claude-login",
            "echo ${CLAUDE_CONFIG_DIR:-default}",
            "agent = \"claude\"",
        ) + &check_toml("codex-login", "true", "agent = \"codex\"");
        s.provisioner("mine", &os, &checks);
        let report = s.check("pi-3", None).await.unwrap();
        assert_eq!(report.agents, ["claude", "claude-main"]);
        assert_eq!(
            ids(&report),
            [
                ("claude-login".into(), Some("claude".into()), Status::Pass),
                (
                    "claude-login".into(),
                    Some("claude-main".into()),
                    Status::Pass
                ),
            ]
        );
        let home = s.fake.home.display();
        // An agent that sets none keeps whatever the shell has.
        let inherited = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or("default".into());
        assert_eq!(report.rows[0].first_line, inherited);
        assert_eq!(report.rows[1].first_line, format!("{home}/.claude-main"));
        assert!(
            report
                .text()
                .contains("ok     claude-login (claude-main)  "),
            "{}",
            report.text()
        );
    }

    /// A signed-out claude and agy each get the command that signs them in
    /// from the head; a kind with no interactive login (codex), an agent
    /// that is signed in and a login check that could not run get none.
    #[tokio::test]
    async fn a_signed_out_login_row_names_pastor_login() {
        let flock = "[[flock]]\nname = \"home\"\ndefault = true\nmachines = { pi-3 = 1 }\n\
             [[flock]]\nname = \"work\"\nagent = \"claude-main\"\nmachines = { pi-3 = 1 }\n\
             [[flock]]\nname = \"gem\"\nagent = \"agy\"\nmachines = { pi-3 = 1 }\n\
             [[flock]]\nname = \"cx\"\nagent = \"codex\"\nmachines = { pi-3 = 1 }\n\
             [[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n";
        let mut s = setup(flock);
        let os = format!("os = \"{}\"", here());
        // claude-main is signed in; claude and agy are not; the codex
        // check fails too; a check that errors (exit 2) is not "signed out".
        let checks = check_toml(
            "claude-login",
            "case \"$CLAUDE_CONFIG_DIR\" in *claude-main) exit 0;; *) echo 'Not logged in'; exit 1;; esac",
            "agent = \"claude\"",
        ) + &check_toml(
            "agy-login",
            "echo 'sign in to agy'; exit 1",
            "agent = \"agy\"",
        ) + &check_toml("codex-login", "exit 1", "agent = \"codex\"");
        s.provisioner("mine", &os, &checks);
        let report = s.check("pi-3", None).await.unwrap();
        let row = |id: &str, agent: &str| {
            report
                .rows
                .iter()
                .find(|r| r.id == id && r.agent.as_deref() == Some(agent))
                .unwrap_or_else(|| panic!("no {id} for {agent}: {:?}", report.rows))
        };
        let claude = row("claude-login", "claude");
        assert_eq!(claude.status, Status::Fail);
        assert!(
            claude
                .reason
                .as_deref()
                .unwrap()
                .contains("`pastor login pi-3 claude`"),
            "{claude:?}"
        );
        assert_eq!(
            claude.first_line, "Not logged in",
            "the check's own line stays"
        );
        let agy = row("agy-login", "agy");
        assert!(
            agy.reason
                .as_deref()
                .unwrap()
                .contains("`pastor login pi-3 agy`"),
            "{agy:?}"
        );
        assert_eq!(row("claude-login", "claude-main").reason, None);
        assert_eq!(row("codex-login", "codex").reason, None);
        let text = report.text();
        assert!(text.contains("pastor login pi-3 claude"), "{text}");
        assert!(!text.contains("pastor login pi-3 codex"), "{text}");
    }

    /// Every Claude agent a task can get on the machine gets an `inbox` row,
    /// after the checks' own rows; a signed-out one is skipped.
    #[cfg(feature = "fake-herdr")]
    #[tokio::test]
    async fn each_claude_agent_gets_an_inbox_row() {
        let flock = "[[flock]]\nname = \"home\"\ndefault = true\nmachines = { pi-3 = 1 }\n\
             [[flock]]\nname = \"work\"\nagent = \"claude-main\"\nmachines = { pi-3 = 1 }\n\
             [[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n";
        let s = setup(flock);
        let config = Config {
            defaults: &s.config.defaults,
            agents: &s.config.agents,
            models: &s.config.models,
            profiles: &s.config.profiles,
            harnesses: crate::harness_manifest::Harnesses::builtin(),
            herdr_pin: None,
            head_address: None,
        };
        let fake = crate::herdr::fake::FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let m = s.flock.get("pi-3").unwrap();
        let login = Row {
            id: "claude-login".into(),
            agent: Some("claude-main".into()),
            status: Status::Fail,
            first_line: String::new(),
            reason: None,
        };
        let rows = inbox_rows_on(&fake, m, &s.flock, &config, &[login]).await;
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.agent.as_deref().unwrap(), r.status))
            .collect();
        assert_eq!(
            got,
            [
                ("inbox", "claude", Status::Pass),
                ("inbox", "claude-main", Status::Skipped)
            ]
        );
    }

    /// A flock whose agent is of a kind the machine has no agent for gives
    /// no agent there: dispatch would not send its task to the machine, not
    /// even to the machine's own agent.
    #[tokio::test]
    async fn an_agent_the_machine_cannot_run_is_left_out() {
        let flock = "[[flock]]\nname = \"work\"\ndefault = true\nagent = \"codex\"\n\
             machines = { pi-3 = 1 }\n\
             [[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\nagent = \"claude-main\"\n";
        let mut s = setup(flock);
        let os = format!("os = \"{}\"", here());
        let checks = check_toml("claude-login", "true", "agent = \"claude\"")
            + &check_toml("codex-login", "true", "agent = \"codex\"");
        s.provisioner("mine", &os, &checks);
        let report = s.check("pi-3", None).await.unwrap();
        assert_eq!(report.agents, Vec::<String>::new());
        assert!(report.rows.is_empty(), "{report:?}");
    }

    /// A check for an account runs only when one of the machine's flocks
    /// lists it.
    #[tokio::test]
    async fn an_account_check_runs_only_for_a_flock_that_lists_it() {
        let mut s = setup(ONE_MACHINE);
        let os = format!("os = \"{}\"", here());
        s.provisioner(
            "mine",
            &os,
            &check_toml("gh", "true", "account = \"github\""),
        );
        assert!(s.check("pi-3", None).await.unwrap().rows.is_empty());
        let mut s2 = setup(&ONE_MACHINE.replace(
            "machines = { pi-3 = 2 }\n",
            "machines = { pi-3 = 2 }\naccounts = [\"github\"]\n",
        ));
        s2.provisioner(
            "mine",
            &os,
            &check_toml("gh", "true", "account = \"github\""),
        );
        assert_eq!(s2.check("pi-3", None).await.unwrap().rows.len(), 1);
    }

    /// `--json` prints the report as it is: the machine, the provisioner
    /// and its version, the agents and a row per check.
    #[tokio::test]
    async fn the_json_shape() {
        let mut s = setup(ONE_MACHINE);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "exit 2", ""));
        let report = s.check("pi-3", None).await.unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "machine": "pi-3",
                "provisioner": "mine",
                "version": "0.2.0",
                "agents": ["claude"],
                "rows": [{
                    "id": "git", "agent": null, "status": "error",
                    "first_line": "", "reason": "exit 2",
                }],
            })
        );
        let back: Report = serde_json::from_value(json).unwrap();
        assert_eq!(back, report);
        // A check that could not run says the machine is missing something
        // as a failed one does.
        assert!(!report.ok());
        assert_eq!(report.failed().count(), 1);
        let event = report.event_detail(&report.rows[0]);
        assert_eq!(event["check"], "git");
        assert_eq!(event["status"], "error");
        assert_eq!(event["provisioner"], "mine");
    }

    /// A `gpg` for git to run, signing with `sign` and verifying with
    /// `verify` (shell, run with git's own arguments), and a `~/.gitconfig`
    /// in `home` that names it and a key. It reads all of its stdin first:
    /// git writes the signed data to it, and a `gpg` that exits before that
    /// is written kills git with SIGPIPE, silently, under load.
    fn git_signs_with(home: &Path, sign: &str, verify: &str) {
        git_config_with(home, sign, verify, "\tsigningkey = ABCDEF\n");
    }

    /// `git_signs_with`, with `user` the extra lines of its `[user]`
    /// section (a `signingkey`, or none) and the gpg stand-in's config.
    fn git_config_with(home: &Path, sign: &str, verify: &str, user: &str) {
        use std::os::unix::fs::PermissionsExt;
        let gpg = home.join("stand-in-gpg");
        std::fs::write(
            &gpg,
            format!("#!/bin/sh\ncat >/dev/null\ncase \" $* \" in\n*\" --verify \"*)\n{verify}\n;;\n*)\n{sign}\n;;\nesac\n"),
        )
        .unwrap();
        std::fs::set_permissions(&gpg, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            format!(
                "[user]\n\tname = Check\n\temail = check@example.com\n{user}\
                 [gpg]\n\tprogram = {}\n",
                gpg.display()
            ),
        )
        .unwrap();
    }

    /// What git reads from `gpg` for a signature made.
    const SIGNS: &str = "echo '[GNUPG:] SIG_CREATED D 1 10 00 1 ABCDEF0123456789' >&2\n\
         printf -- '-----BEGIN PGP SIGNATURE-----\\n\\nZmFrZQ==\\n-----END PGP SIGNATURE-----\\n'";

    /// What git reads from `gpg --verify` for a good signature.
    const VERIFIES: &str = "echo '[GNUPG:] NEWSIG'\n\
         echo '[GNUPG:] GOODSIG ABCDEF0123456789 Check <check@example.com>'\n\
         echo '[GNUPG:] VALIDSIG ABCDEF0123456789 2026-01-01 1 0 4 0 1 10 00 ABCDEF0123456789'\n\
         echo '[GNUPG:] TRUST_ULTIMATE 0 pgp'";

    /// The `signing` row of a check of `pi-3` through the setup's own fake.
    async fn signing_of(s: &mut Setup) -> Row {
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        let report = s
            .check_raw_on(s.fake.clone(), &catalog, "pi-3", None)
            .await
            .unwrap();
        report
            .rows
            .into_iter()
            .find(|r| r.id == SIGNING_ROW)
            .expect("a signing row")
    }

    /// A commit that signs and verifies passes. It is made in a run
    /// directory under the machine's pastor cache, which is gone again with
    /// its empty parents, and no script reads a key.
    #[tokio::test]
    async fn a_commit_that_signs_and_verifies_passes() {
        let mut s = setup(ONE_MACHINE);
        git_signs_with(&s.fake.home, SIGNS, VERIFIES);
        let row = signing_of(&mut s).await;
        assert_eq!(
            (row.status, row.first_line.as_str(), row.reason),
            (Status::Pass, "a commit here signs and verifies", None)
        );
        assert!(!s.fake.home.join(".cache/pastor").exists());
        let scripts = s.fake.scripts.lock().unwrap();
        let commit = scripts
            .iter()
            .find(|x| x.contains("pastor signing check"))
            .expect("a commit was made");
        let dir = format!("{}/.cache/pastor/setup/", s.fake.home.display());
        assert!(commit.contains(&format!("cd '{dir}")), "{commit}");
        for script in scripts.iter() {
            for key in [".gnupg", ".ssh", "ABCDEF", "id_ed25519"] {
                assert!(!script.contains(key), "{key} in {script}");
            }
        }
    }

    /// A commit that does not sign fails with git's own words, and the
    /// repo it was made in is removed all the same.
    #[tokio::test]
    async fn a_commit_that_does_not_sign_fails_with_gits_words() {
        let mut s = setup(ONE_MACHINE);
        git_signs_with(
            &s.fake.home,
            "echo 'gpg: signing failed: No secret key' >&2; exit 2",
            VERIFIES,
        );
        let row = signing_of(&mut s).await;
        assert_eq!(
            (row.status, row.first_line.as_str()),
            (Status::Fail, "a commit here does not sign")
        );
        let why = row.reason.expect("git's words");
        assert!(why.contains("No secret key"), "{why}");
        assert!(why.contains("failed to sign"), "{why}");
        assert!(!s.fake.home.join(".cache/pastor").exists());
    }

    /// A commit that signs but whose signature does not verify (git cannot
    /// check it, here for want of the public key) fails as that, not as a
    /// commit that does not sign.
    #[tokio::test]
    async fn a_commit_that_signs_but_does_not_verify_fails() {
        let mut s = setup(ONE_MACHINE);
        git_signs_with(
            &s.fake.home,
            SIGNS,
            "echo \"gpg: Can't check signature: No public key\" >&2; exit 2",
        );
        let row = signing_of(&mut s).await;
        assert_eq!(
            (row.status, row.first_line.as_str()),
            (Status::Fail, "a signed commit here does not verify")
        );
        let why = row.reason.expect("git's words");
        assert!(why.contains("No public key"), "{why}");
        assert!(!s.fake.home.join(".cache/pastor").exists());
    }

    /// A machine with no git fails, saying so.
    #[tokio::test]
    async fn a_machine_without_git_fails_the_signing_row() {
        let mut s = setup(ONE_MACHINE);
        let bin = s.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Every tool of the host's PATH but git.
        for dir in ["/usr/bin", "/bin"] {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let name = entry.file_name();
                if !name.to_string_lossy().starts_with("git") {
                    let _ = std::os::unix::fs::symlink(entry.path(), bin.join(&name));
                }
            }
        }
        s.fake = Arc::new(Fake::new(
            &s.fake.home,
            Some(bin.to_string_lossy().to_string()),
        ));
        let row = signing_of(&mut s).await;
        assert_eq!(
            (row.status, row.first_line.as_str()),
            (Status::Fail, "git is not on the machine's PATH")
        );
        assert!(!s.fake.home.join(".cache/pastor").exists());
    }

    /// A signer that never answers (a passphrase it cannot be given) is a
    /// row that could not run, not a check that hangs; the repo goes.
    #[tokio::test]
    async fn a_signer_that_never_answers_is_an_error_row() {
        let s = setup(ONE_MACHINE);
        git_signs_with(&s.fake.home, "sleep 5", VERIFIES);
        let row = signing_row(
            s.fake.clone(),
            "pi-3",
            std::time::Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            (row.status, row.first_line.as_str()),
            (Status::Error, "could not try a signed commit")
        );
        assert!(row.reason.unwrap().contains("passphrase"));
        assert!(!s.fake.home.join(".cache/pastor").exists());
    }

    /// A failed signing row fails the check and is raised as an event like
    /// any other failed row.
    #[tokio::test]
    async fn a_machine_that_cannot_sign_is_not_ok() {
        let mut s = setup(ONE_MACHINE);
        git_signs_with(&s.fake.home, "exit 2", VERIFIES);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        let report = s
            .check_raw_on(s.fake.clone(), &catalog, "pi-3", None)
            .await
            .unwrap();
        assert!(!report.ok(), "{report:?}");
        let failed: Vec<&str> = report.failed().map(|r| r.id.as_str()).collect();
        assert_eq!(failed, [SIGNING_ROW]);
        assert!(
            report
                .text()
                .contains("fail   signing  a commit here does not sign")
        );
    }

    const GH_FLOCK: &str = "[[flock]]\nname = \"work\"\ndefault = true\n\
         machines = { pi-3 = 2 }\naccounts = [\"github\"]\n\
         [[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n";

    /// A `gh` in the machine's `~/.local/bin` that prints `status` for
    /// `auth status` (as gh does, the token masked) and, for `api -i`, the
    /// response headers with an expiry line for each `(host, when)`.
    fn gh_says(home: &Path, status: &str, expiry: &[(&str, String)]) {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let mut api = String::new();
        for (host, when) in expiry {
            api.push_str(&format!(
                "[ \"$4\" = '{host}' ] && printf 'Github-Authentication-Token-Expiration: {when}\\r\\n'\n"
            ));
        }
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\ncase \"$1\" in\nauth) cat <<'EOS'\n{status}\nEOS\n;;\n\
                 api) printf 'HTTP/2.0 200 OK\\r\\nContent-Type: application/json\\r\\n'\n{api}\
                 printf '\\r\\n{{}}\\n';;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn logged_in(host: &str, account: &str) -> String {
        format!(
            "{host}\n  ✓ Logged in to {host} account {account} (keyring)\n  - Active account: true\n  \
             - Git operations protocol: https\n  - Token: gho_SECRETSECRET\n  - Token scopes: 'repo'"
        )
    }

    /// The `gh-auth` rows of a check of `pi-3`, as (status, text, reason).
    async fn gh_rows(s: &mut Setup) -> Vec<(Status, String, Option<String>)> {
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        let report = s
            .check_raw_on(s.fake.clone(), &catalog, "pi-3", None)
            .await
            .unwrap();
        assert!(
            !serde_json::to_string(&report).unwrap().contains("SECRET"),
            "the token is in the report"
        );
        report
            .rows
            .iter()
            .filter(|r| r.id == GH_AUTH_ROW)
            .map(|r| (r.status, r.first_line.clone(), r.reason.clone()))
            .collect()
    }

    fn when_in(hours: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::hours(hours))
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string()
    }

    /// A login that works passes, with or without an expiry, and the token
    /// is never in the report; a month out is no warning.
    #[tokio::test]
    async fn a_gh_login_that_works_passes() {
        let mut s = setup(GH_FLOCK);
        gh_says(&s.fake.home, &logged_in("github.com", "alice"), &[]);
        assert_eq!(
            gh_rows(&mut s).await,
            [(
                Status::Pass,
                "github.com as alice: logged in".to_string(),
                None
            )]
        );
        let when = when_in(30 * 24 + 12);
        gh_says(
            &s.fake.home,
            &logged_in("github.com", "alice"),
            &[("github.com", when.clone())],
        );
        let rows = gh_rows(&mut s).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, Status::Pass, "{rows:?}");
        assert!(
            rows[0]
                .1
                .starts_with("github.com as alice: logged in, the token expires 20")
                && rows[0].1.ends_with("(in 30 days)"),
            "{rows:?}"
        );
    }

    /// A token that expires within a week is a warning that names the fix
    /// and does not fail the check.
    #[tokio::test]
    async fn a_gh_token_about_to_expire_warns() {
        let mut s = setup(GH_FLOCK);
        gh_says(
            &s.fake.home,
            &logged_in("github.com", "alice"),
            &[("github.com", when_in(3 * 24 + 12))],
        );
        let rows = gh_rows(&mut s).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, Status::Warn, "{rows:?}");
        assert!(rows[0].1.ends_with("(in 3 days)"), "{rows:?}");
        assert!(
            rows[0].2.as_deref().unwrap().contains("--with-token"),
            "{rows:?}"
        );
        // Six days out is under the week, seven is not.
        gh_says(
            &s.fake.home,
            &logged_in("github.com", "alice"),
            &[("github.com", when_in(7 * 24 + 12))],
        );
        assert_eq!(gh_rows(&mut s).await[0].0, Status::Pass);
        gh_says(
            &s.fake.home,
            &logged_in("github.com", "alice"),
            &[("github.com", when_in(6 * 24 + 12))],
        );
        assert_eq!(gh_rows(&mut s).await[0].0, Status::Warn);
    }

    /// Each host is its own row; a host whose token gh says does not work
    /// fails, and an account that is not the active one is left out.
    #[tokio::test]
    async fn gh_is_read_per_host() {
        let mut s = setup(GH_FLOCK);
        let status = format!(
            "{}\n{}\n  X Failed to log in to ghe.example.com account bob (keyring)\n  \
             - Active account: true\n  - The token in keyring is invalid.\n  \
             ✓ Logged in to github.com account other (keyring)\n  - Active account: false",
            logged_in("github.com", "alice"),
            "ghe.example.com"
        );
        gh_says(
            &s.fake.home,
            &status,
            &[("github.com", when_in(40 * 24 + 12))],
        );
        let rows = gh_rows(&mut s).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].0, Status::Pass, "{rows:?}");
        assert!(
            rows[0].1.starts_with("github.com as alice") && rows[0].1.ends_with("(in 40 days)"),
            "{rows:?}"
        );
        assert_eq!(rows[1].0, Status::Fail, "{rows:?}");
        assert_eq!(
            rows[1].1, "ghe.example.com as bob: the token does not work",
            "{rows:?}"
        );
        assert!(
            rows[1]
                .2
                .as_deref()
                .unwrap()
                .contains("--hostname ghe.example.com"),
            "{rows:?}"
        );
    }

    /// No login at all is one warning (the provisioner's own check fails for
    /// github.com); a machine without gh gets no row; a flock that lists no
    /// github account gets none either.
    #[tokio::test]
    async fn gh_not_logged_in_or_not_there() {
        let mut s = setup(GH_FLOCK);
        gh_says(
            &s.fake.home,
            "You are not logged into any GitHub hosts. To log in, run: gh auth login",
            &[],
        );
        let rows = gh_rows(&mut s).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0].0, rows[0].1.as_str()),
            (Status::Warn, "gh is not logged in to any host")
        );

        // No gh: every tool of the host's PATH but gh.
        let bin = s.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for dir in ["/usr/bin", "/bin"] {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                if entry.file_name() != "gh" {
                    let _ = std::os::unix::fs::symlink(entry.path(), bin.join(entry.file_name()));
                }
            }
        }
        let home = s.root.join("nogh-home");
        std::fs::create_dir_all(&home).unwrap();
        s.fake = Arc::new(Fake::new(&home, Some(bin.to_string_lossy().to_string())));
        assert_eq!(gh_rows(&mut s).await, []);

        // A flock without the account never asks.
        let mut s = setup(ONE_MACHINE);
        gh_says(&s.fake.home, &logged_in("github.com", "alice"), &[]);
        assert_eq!(gh_rows(&mut s).await, []);
        assert!(
            !s.fake
                .scripts
                .lock()
                .unwrap()
                .iter()
                .any(|x| x.contains("gh auth status")),
            "gh was asked"
        );
    }

    /// The probe never asks gh for the token.
    #[test]
    fn the_gh_probe_never_asks_for_the_token() {
        let script = gh_script();
        assert!(!script.contains("auth token"), "{script}");
        assert!(!script.contains("show-token"), "{script}");
    }

    /// Where git's config names no signing key and leaves `commit.gpgsign`
    /// unset, a commit that does not sign is a warning, not a failure:
    /// signing was never set up. One that signs anyway passes, and a
    /// `commit.gpgsign` on its own counts as set up.
    #[tokio::test]
    async fn unconfigured_signing_warns_and_broken_signing_fails() {
        let mut s = setup(ONE_MACHINE);
        let cannot = "echo 'gpg: signing failed: No secret key' >&2; exit 2";
        git_config_with(&s.fake.home, cannot, VERIFIES, "");
        let row = signing_of(&mut s).await;
        assert_eq!(
            (row.status, row.first_line.as_str()),
            (Status::Warn, "signing is not set up here")
        );
        assert!(row.reason.unwrap().contains("signingkey"));
        let report = Report {
            machine: "pi-3".into(),
            provisioner: None,
            version: None,
            agents: vec![],
            rows: vec![signing_of(&mut s).await],
        };
        assert!(report.ok(), "a warning does not fail the check");

        // Signing that works without a key in the config passes.
        git_config_with(&s.fake.home, SIGNS, VERIFIES, "");
        assert_eq!(signing_of(&mut s).await.status, Status::Pass);

        // Signing turned on and broken is a failure, with or without a key.
        git_config_with(&s.fake.home, cannot, VERIFIES, "");
        let mut config = std::fs::read_to_string(s.fake.home.join(".gitconfig")).unwrap();
        config.push_str("[commit]\n\tgpgsign = true\n");
        std::fs::write(s.fake.home.join(".gitconfig"), config).unwrap();
        let row = signing_of(&mut s).await;
        assert_eq!(
            (row.status, row.first_line.as_str()),
            (Status::Fail, "a commit here does not sign")
        );
        git_config_with(&s.fake.home, cannot, VERIFIES, "\tsigningkey = ABCDEF\n");
        assert_eq!(signing_of(&mut s).await.status, Status::Fail);
        // The key itself is not in the report.
        assert!(!signing_of(&mut s).await.reason.unwrap().contains("ABCDEF"));
    }

    /// The `profile` and `opencode-config` rows of `s`'s check of pi-3.
    async fn rows_of(s: &Setup, id: &str) -> Vec<Row> {
        let report = s.check("pi-3", None).await.unwrap();
        report.rows.into_iter().filter(|r| r.id == id).collect()
    }

    /// Each agent a task can get on the machine has a `profile` row naming
    /// the profile it runs under and where that comes from, `ok` when the
    /// profile renders for its harness as a task start would render it, and
    /// `fail` with the refusal when it does not. An agent no profile applies
    /// to has no row.
    #[tokio::test]
    async fn the_profile_row_says_what_each_agent_runs_under() {
        let mut s = setup(ONE_MACHINE);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        assert_eq!(rows_of(&s, PROFILE_ROW).await, [], "no profile, no row");

        s.config.defaults.profile = Some("develop".into());
        let rows = rows_of(&s, PROFILE_ROW).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].agent.as_deref(), Some("claude"));
        assert_eq!(rows[0].status, Status::Pass, "{rows:?}");
        assert_eq!(
            rows[0].first_line,
            "develop, from defaults, renders for claude"
        );

        // The machine's own profile names the profile, and the text says so.
        s.flock = Flock::parse(
            Path::new("flock.toml"),
            &format!("{ONE_MACHINE}profile = \"review\"\n"),
        )
        .unwrap();
        let report = s.check("pi-3", None).await.unwrap();
        assert!(
            report.text().contains(
                "ok     profile (claude)  review, from machine pi-3, renders for claude\n"
            ),
            "{}",
            report.text()
        );

        // Args that pick a permission mode meet the profile's: a task there
        // would be refused, so the row fails with the refusal.
        s.config.defaults.agent_args = vec!["--permission-mode".into(), "plan".into()];
        let report = s.check("pi-3", None).await.unwrap();
        assert!(!report.ok());
        let row = report.rows.iter().find(|r| r.id == PROFILE_ROW).unwrap();
        assert_eq!(row.status, Status::Fail);
        assert_eq!(
            row.first_line,
            "review, from machine pi-3, does not render for claude"
        );
        let reason = row.reason.as_deref().unwrap();
        assert!(
            reason.starts_with(&format!("{}: ", crate::config::PROFILE_ARGS_CONFLICT)),
            "{reason}"
        );
        assert!(reason.contains("--permission-mode"), "{reason}");
    }

    /// `unrestricted` from a flock on a machine whose own profile is not
    /// is what a task there is refused for, so the row fails with it.
    #[tokio::test]
    async fn an_unrestricted_profile_the_machine_does_not_allow_fails() {
        let flock = "[[flock]]\nname = \"work\"\ndefault = true\nprofile = \"unrestricted\"\n\
             machines = { pi-3 = 2 }\n[[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n\
             profile = \"develop\"\n";
        let mut s = setup(flock);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let rows = rows_of(&s, PROFILE_ROW).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].status, Status::Fail);
        assert_eq!(
            rows[0].first_line,
            "unrestricted, from flock work, does not render for claude"
        );
        assert!(
            rows[0]
                .reason
                .as_deref()
                .unwrap()
                .starts_with(crate::config::profile::PROFILE_NOT_ALLOWED),
            "{rows:?}"
        );
    }

    /// A machine an opencode agent can land on gets an `opencode-config`
    /// row: `ok` when opencode's config there has nothing a task under a
    /// profile is refused for, saying which of its keys such a task denies;
    /// `fail`, naming each file and key, when it has rules pastor cannot
    /// override, or `warn` when no opencode agent there runs under a
    /// profile. A machine with no opencode agent has no row and is not
    /// asked.
    #[tokio::test]
    async fn the_opencode_row_reads_the_config_a_task_start_reads() {
        let mut s = setup(ONE_MACHINE);
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        assert_eq!(rows_of(&s, OPENCODE_ROW).await, [], "no opencode agent");
        assert!(
            !s.fake
                .scripts
                .lock()
                .unwrap()
                .iter()
                .any(|x| x.contains("opencode")),
            "opencode's config was read"
        );

        s.config.defaults.agent = "opencode".into();
        let rows = rows_of(&s, OPENCODE_ROW).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].agent, None);
        assert_eq!(rows[0].status, Status::Pass, "{rows:?}");
        assert_eq!(rows[0].first_line, "no permission rules of its own");

        let own = s.root.join("home/.config/opencode/opencode.json");
        std::fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::fs::write(&own, r#"{"permission":{"bash":"allow","edit":"ask"}}"#).unwrap();
        let rows = rows_of(&s, OPENCODE_ROW).await;
        assert_eq!(rows[0].status, Status::Pass, "{rows:?}");
        assert_eq!(
            rows[0].first_line,
            "sets bash, edit, which a task under a profile denies"
        );

        // Rules under an agent are past what pastor can override.
        std::fs::write(
            &own,
            r#"{"agent":{"build":{"permission":{"bash":"allow"}}}}"#,
        )
        .unwrap();
        let rows = rows_of(&s, OPENCODE_ROW).await;
        assert_eq!(rows[0].status, Status::Warn, "no profile here: {rows:?}");
        s.config.defaults.profile = Some("develop".into());
        let rows = rows_of(&s, OPENCODE_ROW).await;
        assert_eq!(rows[0].status, Status::Fail, "{rows:?}");
        assert!(
            rows[0]
                .first_line
                .contains(&own.to_string_lossy().to_string()),
            "{rows:?}"
        );
        assert!(
            rows[0].reason.as_deref().unwrap().starts_with(&format!(
                "{}: ",
                crate::config::opencode::OPENCODE_PERMISSIONS_CONFLICT
            )),
            "{rows:?}"
        );

        // The managed config is merged after pastor's: a key there is named
        // with its file.
        std::fs::remove_file(&own).unwrap();
        let managed = s.root.join("managed/opencode.json");
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::write(&managed, r#"{"tools":{"bash":true}}"#).unwrap();
        let rows = rows_of(&s, OPENCODE_ROW).await;
        assert_eq!(rows[0].status, Status::Fail, "{rows:?}");
        assert_eq!(
            rows[0].first_line,
            format!(
                "has permission rules of its own: {} (tools)",
                managed.display()
            )
        );
    }

    fn accounts_flock(accounts: &str) -> String {
        format!(
            "[[flock]]\nname = \"home\"\ndefault = true\nmachines = {{ pi-3 = 1 }}\naccounts = {accounts}\n\
             [[flock]]\nname = \"work\"\nagent = \"claude-main\"\nmachines = {{ pi-3 = 1 }}\naccounts = {accounts}\n\
             [[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n"
        )
    }

    /// A `claude` in the machine's `~/.local/bin` that says it is signed in
    /// as `me@example.com`, or as `work@example.com` for the agent whose
    /// config dir is the machine's `~/.claude-main` (a `~` expanded there).
    fn claude_signed_in(home: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("claude");
        std::fs::write(
            &claude,
            format!(
                "#!/bin/sh\necho 'Login method: Claude Max account'\n\
                 case \"$CLAUDE_CONFIG_DIR\" in\n\"{}/.claude-main\") echo 'Email: Work@example.com';;\n\
                 *) echo 'Email: me@example.com';;\nesac\n",
                home.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    async fn account_rows(s: &mut Setup, id: &str) -> Vec<(String, Status, String)> {
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        let report = s
            .check_raw_on(s.fake.clone(), &catalog, "pi-3", None)
            .await
            .unwrap();
        report
            .rows
            .iter()
            .filter(|r| r.id == id)
            .map(|r| {
                (
                    r.agent.clone().unwrap_or_default(),
                    r.status,
                    r.first_line.clone(),
                )
            })
            .collect()
    }

    /// With `accounts = { claude = ... }` each Claude agent's row compares
    /// the account it is signed in as: ok when it matches (without case),
    /// a failure naming both when it does not; with the list form, or no
    /// name for claude, no such row, and the account is never asked.
    #[tokio::test]
    async fn a_claude_agent_signed_in_to_the_wrong_account_fails() {
        let mut s = setup(&accounts_flock("{ claude = \"me@example.com\" }"));
        claude_signed_in(&s.fake.home);
        assert_eq!(
            account_rows(&mut s, CLAUDE_ACCOUNT_ROW).await,
            [
                (
                    "claude".into(),
                    Status::Pass,
                    "signed in as me@example.com".into()
                ),
                (
                    "claude-main".into(),
                    Status::Fail,
                    "signed in as Work@example.com, the flock expects me@example.com".into()
                ),
            ]
        );
        // Either of two names is right, and case does not matter.
        let mut s = setup(&accounts_flock("{ claude = \"ME@example.com\" }"));
        claude_signed_in(&s.fake.home);
        let rows = account_rows(&mut s, CLAUDE_ACCOUNT_ROW).await;
        assert_eq!(rows[0].1, Status::Pass, "{rows:?}");

        for unset in ["[\"claude\"]", "{ github = \"alice\" }"] {
            let mut s = setup(&accounts_flock(unset));
            claude_signed_in(&s.fake.home);
            assert_eq!(
                account_rows(&mut s, CLAUDE_ACCOUNT_ROW).await,
                [],
                "{unset}"
            );
            assert!(
                !s.fake
                    .scripts
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|x| x.contains("claude auth status")),
                "{unset}: asked"
            );
        }
    }

    /// A claude that does not say who is signed in is a warning, and an
    /// agent whose login row already failed gets no account row.
    #[tokio::test]
    async fn an_unreadable_claude_account_warns_and_a_failed_login_is_left_to_its_row() {
        let mut s = setup(&accounts_flock("{ claude = \"me@example.com\" }"));
        {
            use std::os::unix::fs::PermissionsExt;
            let bin = s.fake.home.join(".local/bin");
            std::fs::create_dir_all(&bin).unwrap();
            let claude = bin.join("claude");
            std::fs::write(&claude, "#!/bin/sh\necho 'Login method: API key'\n").unwrap();
            std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let rows = account_rows(&mut s, CLAUDE_ACCOUNT_ROW).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.iter().all(|r| r.1 == Status::Warn), "{rows:?}");

        // The provisioner's login row fails for `claude`: no account row.
        let mut s = setup(&accounts_flock("{ claude = \"me@example.com\" }"));
        claude_signed_in(&s.fake.home);
        let os = format!("os = \"{}\"", here());
        s.provisioner(
            "mine",
            &os,
            &check_toml("claude-login", "exit 1", "agent = \"claude\""),
        );
        let catalog = Catalog::of(s.provisioners.clone());
        let report = s
            .check_raw_on(s.fake.clone(), &catalog, "pi-3", None)
            .await
            .unwrap();
        let agents: Vec<_> = report
            .rows
            .iter()
            .filter(|r| r.id == CLAUDE_ACCOUNT_ROW)
            .map(|r| r.agent.clone().unwrap())
            .collect();
        assert!(agents.is_empty(), "{agents:?}");
    }

    /// `accounts = { github = ... }` compares the account `gh` is signed in
    /// as on github.com: a match passes, another account fails naming both,
    /// and the list form (no name) passes as before.
    #[tokio::test]
    async fn gh_signed_in_to_the_wrong_account_fails() {
        let flock = |accounts: &str| {
            format!(
                "[[flock]]\nname = \"work\"\ndefault = true\nmachines = {{ pi-3 = 2 }}\naccounts = {accounts}\n\
                 [[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n"
            )
        };
        let mut s = setup(&flock("{ github = \"Alice\" }"));
        gh_says(&s.fake.home, &logged_in("github.com", "alice"), &[]);
        let rows = gh_rows(&mut s).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, Status::Pass, "{rows:?}");

        gh_says(&s.fake.home, &logged_in("github.com", "bob"), &[]);
        let rows = gh_rows(&mut s).await;
        assert_eq!(rows[0].0, Status::Fail, "{rows:?}");
        assert_eq!(
            rows[0].1, "github.com as bob: the flock expects Alice",
            "{rows:?}"
        );
        assert!(
            rows[0].2.as_deref().unwrap().contains("gh auth login"),
            "{rows:?}"
        );

        // No name: whoever is signed in passes, as before.
        let mut s = setup(GH_FLOCK);
        gh_says(&s.fake.home, &logged_in("github.com", "bob"), &[]);
        assert_eq!(gh_rows(&mut s).await[0].0, Status::Pass);
        let mut s = setup(&flock("{ claude = \"me@example.com\" }"));
        gh_says(&s.fake.home, &logged_in("github.com", "bob"), &[]);
        assert_eq!(gh_rows(&mut s).await, []);
    }

    /// An `ssh` in the machine's `~/.local/bin` that stands in for the head:
    /// it wants the batch options and the head's address, then answers the
    /// ping on its stdin as the agent bridge does (`pong`) or, with `says`,
    /// prints that and fails as ssh does.
    fn ssh_to_head(home: &Path, head: &str, says: Option<&str>) {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ssh = bin.join("ssh");
        let reply = match says {
            None => "cat >/dev/null; echo '{\"kind\":\"pong\",\"data\":{\"version\":\"0.10.0\"}}'; exit 0"
                .to_string(),
            Some(words) => format!("cat >/dev/null; echo '{words}'; exit 255"),
        };
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\ncase \" $* \" in\n*\" BatchMode=yes \"*\" ConnectTimeout=\"*\" -- {head} \"*) ;;\n\
                 *) echo \"unexpected ssh arguments: $*\" >&2; exit 2;;\nesac\n{reply}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    async fn reach_rows(s: &mut Setup) -> Vec<(Status, String, Option<String>)> {
        let os = format!("os = \"{}\"", here());
        s.provisioner("mine", &os, &check_toml("git", "true", ""));
        let catalog = Catalog::of(s.provisioners.clone());
        let report = s
            .check_raw_on(s.fake.clone(), &catalog, "pi-3", None)
            .await
            .unwrap();
        report
            .rows
            .iter()
            .filter(|r| r.id == REACH_ROW)
            .map(|r| (r.status, r.first_line.clone(), r.reason.clone()))
            .collect()
    }

    /// An ssh machine with a `head_address` gets a `reach` row: ok when the
    /// head answers a batch-mode ping over ssh, a failure naming the
    /// destination and ssh's words when it does not. No `head_address`, or a
    /// machine that is not reached by ssh, gets none.
    #[tokio::test]
    async fn the_reach_row_says_whether_the_machine_gets_to_the_head() {
        let mut s = setup(ONE_MACHINE);
        s.head_address = Some("user@head.example".into());
        ssh_to_head(&s.fake.home, "user@head.example", None);
        assert_eq!(
            reach_rows(&mut s).await,
            [(
                Status::Pass,
                "pi-3 reaches the head at user@head.example".to_string(),
                None
            )]
        );

        ssh_to_head(
            &s.fake.home,
            "user@head.example",
            Some("user@head.example: Permission denied (publickey)."),
        );
        let rows = reach_rows(&mut s).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, Status::Fail);
        assert_eq!(rows[0].1, "ssh to user@head.example from pi-3 failed");
        let why = rows[0].2.as_deref().unwrap();
        assert!(why.contains("Permission denied (publickey)"), "{why}");
        assert!(why.contains("machine authorized-key pi-3"), "{why}");

        // Something that answers but is not the head is a failure too.
        ssh_to_head(
            &s.fake.home,
            "user@head.example",
            Some("Welcome to a shell"),
        );
        assert_eq!(reach_rows(&mut s).await[0].0, Status::Fail);

        // No head_address: no row.
        s.head_address = None;
        assert_eq!(reach_rows(&mut s).await, []);
        // A local machine reaches the head by its socket: no row.
        let mut s = setup(
            "[[flock]]\nname = \"work\"\ndefault = true\nmachines = { pi-3 = 2 }\n\
             [[machine]]\nname = \"pi-3\"\nlocal = true\n",
        );
        s.head_address = Some("user@head.example".into());
        ssh_to_head(&s.fake.home, "user@head.example", None);
        assert_eq!(reach_rows(&mut s).await, []);
    }
}
