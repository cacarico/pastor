//! Installing a harness on a machine at the version its flock pins
//! (`pastor machine install`), from the sources its manifest names, in
//! order: a release file through the head's artifact cache, npm, or the
//! vendor's installer. Everything runs as the machine's login user; nothing
//! calls sudo or a package manager, and a source whose tool the machine
//! lacks is passed over and named.
//!
//! A release or npm install goes into a dir of its own,
//! `~/.local/share/pastor/harnesses/<id>/<version>/`, and the head reads
//! the version of what it installed there before `~/.local/bin/<binary>`
//! is pointed at it, so an install that fails or brings the wrong version
//! leaves the old one in place. A vendor's installer puts the harness where
//! it likes: pastor saves what `~/.local/bin/<binary>` was first, and puts
//! it back when the version after is not the pin. The manifest's version
//! command has the last word.
//!
//! herdr takes the same path as a release file: its pin is `[artifacts]
//! herdr`, the head fetches it from herdr's release index through the
//! artifact cache, and it is staged, read and linked like a harness, from a
//! manifest of its own that is not an agent's. It is not updated while herdr
//! lists a task's agent (`t-<id>`) on the machine.

use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::artifacts::{self, Artifact, Fetch};
use crate::cli::CliError;
use crate::config::{ArtifactsConfig, Paths, create_private_dir};
use crate::harness;
use crate::harness_manifest::{Harnesses, Install, Manifest};
use crate::herdr::client::ConnectorExt;
use crate::herdr::transport::{Connector, Endpoint, shell_quote};
use crate::provision::run::{self, Facts, Ran, Shell};

/// Where installs pastor makes live on the machine, below `$HOME`: one dir
/// per harness and version.
pub const STAGE_ROOT: &str = ".local/share/pastor/harnesses";

/// Where a harness's command is linked on the machine, below `$HOME`.
pub const LINK_DIR: &str = ".local/bin";

/// Printed before the part of an answer pastor reads, so what the
/// machine's rc files print first is skipped.
const MARK: &str = "pastor-install-5e1f";

/// How long a probe or a version check may take.
const PROBE_TIME: Duration = Duration::from_secs(30);

/// How long one install step on the machine may take: npm and a vendor's
/// installer download the harness.
const INSTALL_TIME: Duration = Duration::from_secs(900);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Already at its pin.
    Kept,
    /// Installed at its pin, or with `--dry-run` would be.
    Installed,
    /// Not installed, on purpose: the release does not match its published
    /// sum.
    Refused,
    /// Not installed: no source fits, or the install did not bring the pin.
    Failed,
    /// Not installed yet, on purpose: the machine is at work and the
    /// install waits for it to be idle. Nothing changed.
    Skipped,
}

/// One pinned harness on the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub kind: String,
    pub pin: String,
    /// The version before, `None` when the harness was not there.
    pub before: Option<String>,
    /// The version after; with `--dry-run`, the one before.
    pub after: Option<String>,
    /// The source used: `release`, `npm` or `script`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub action: Action,
    /// What the install does, in order; with `--dry-run`, what it would do.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub machine: String,
    pub dry_run: bool,
    pub rows: Vec<Row>,
}

impl Report {
    /// Every harness is at its pin, or with `--dry-run` would be.
    pub fn ok(&self) -> bool {
        self.rows
            .iter()
            .all(|r| matches!(r.action, Action::Kept | Action::Installed))
    }

    pub fn text(&self) -> String {
        let mut out = format!(
            "{}{}\n",
            self.machine,
            if self.dry_run { " (dry run)" } else { "" }
        );
        for r in &self.rows {
            let word = match r.action {
                Action::Kept => "kept",
                Action::Installed if self.dry_run => "would install",
                Action::Installed => "installed",
                Action::Refused => "refused",
                Action::Failed => "failed",
                Action::Skipped => "skipped",
            };
            let before = r.before.as_deref().unwrap_or("none");
            let mut line = match r.action {
                Action::Kept => format!("  {word:<13}  {} {before}, pinned {}", r.kind, r.pin),
                _ => format!(
                    "  {word:<13}  {} {before} -> {}",
                    r.kind,
                    r.after
                        .as_deref()
                        .filter(|_| !self.dry_run)
                        .unwrap_or(r.pin.as_str())
                ),
            };
            if let Some(source) = &r.source {
                line.push_str(&format!(" from {source}"));
            }
            if let Some(detail) = &r.detail {
                line.push_str(&format!(": {detail}"));
            }
            out.push_str(&line);
            out.push('\n');
            if self.dry_run {
                for step in &r.steps {
                    out.push_str(&format!("      - {step}\n"));
                }
            }
        }
        out
    }
}

/// Which pinned harnesses `machine install` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Only {
    All,
    /// The kind this agent of the machine's runs.
    Agent(String),
    Kind(String),
}

/// One pinned harness to install.
pub struct Want<'a> {
    pub manifest: &'a Manifest,
    pub pin: &'a str,
}

/// The kind `--kind` names for herdr, which no agent runs.
pub const HERDR: &str = "herdr";

/// herdr to install: `[artifacts] herdr` is the pin.
pub struct HerdrWant<'a> {
    pub pin: &'a str,
    pub artifacts: &'a ArtifactsConfig,
    /// Who is at work on the machine; asked only when herdr is there and is
    /// not the pin.
    pub live: &'a dyn AtWork,
}

pub type AtWorkFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>>;

/// The task agents (`t-<id>`) on a machine: Err says why that could not be
/// told.
pub trait AtWork: Send + Sync {
    fn names(&self) -> AtWorkFuture<'_>;
}

/// The agents herdr lists on the machine that are named as pastor names a
/// task's (`t-<id>`), whatever their task's row says: a stale, failed or
/// done task's agent is still running, and one with no task is an orphan
/// that still holds its pane. Sorted.
pub struct ListsAgents(pub Arc<dyn Connector>);

impl AtWork for ListsAgents {
    fn names(&self) -> AtWorkFuture<'_> {
        Box::pin(async {
            let agents = self.0.agent_list().await.map_err(|e| e.to_string())?;
            let mut names: Vec<String> = crate::machine::orphan_agents(&agents, &[])
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            names.sort();
            Ok(names)
        })
    }
}

/// What pastor needs of herdr to install it as it does a harness: a
/// command, its version and where it is linked. It is nobody's agent, so it
/// has no login, no paths that matter and no install source of its own.
fn herdr_manifest() -> &'static Manifest {
    static HERDR_MANIFEST: LazyLock<Manifest> = LazyLock::new(|| {
        Manifest::parse(
            "id = \"herdr\"\nbinary = \"herdr\"\nbin_dirs = [\"~/.local/bin\"]\n\
             [version]\nargs = [\"--version\"]\n[paths]\nhome = \"~/.config/herdr\"\n",
        )
        .expect("the herdr manifest parses")
    });
    &HERDR_MANIFEST
}

/// The command that prints the herdr version on a machine, or `none`.
pub fn herdr_version_command() -> String {
    herdr_manifest()
        .version_command()
        .expect("the herdr manifest reads a version")
}

/// The version `herdr --version` printed, `None` for `none`.
pub fn herdr_version_from(output: &str) -> Option<String> {
    herdr_manifest().parse_version(output)
}

/// The pinned kinds among `agents` (the agents that can land on the
/// machine), each once, that `only` takes; a pin on a kind no agent there
/// runs is not the machine's to install. Err names what matched nothing.
pub fn wanted<'a>(
    agents: &[run::AgentEnv],
    pins: &'a harness::Pins,
    harnesses: &'a Harnesses,
    only: &Only,
) -> Result<Vec<Want<'a>>, String> {
    let kinds: BTreeSet<&str> = agents
        .iter()
        .filter(|a| match only {
            Only::All => true,
            Only::Agent(name) => &a.name == name,
            Only::Kind(kind) => &a.kind == kind,
        })
        .map(|a| a.kind.as_str())
        .collect();
    let mut out = Vec::new();
    for kind in kinds {
        let (Some(pin), Some(manifest)) = (pins.get(kind), harnesses.get(kind)) else {
            continue;
        };
        out.push(Want { manifest, pin });
    }
    if out.is_empty() {
        return Err(match only {
            Only::All => {
                "none of its agents' harnesses is pinned; pin one with `pins` in flock.toml, or herdr with `herdr` under [artifacts] in pastor.toml"
                    .to_string()
            }
            Only::Agent(name) if !agents.iter().any(|a| &a.name == name) => {
                format!("no agent {name} can land on it")
            }
            Only::Agent(name) => format!("the harness agent {name} runs is not pinned"),
            Only::Kind(kind) if !agents.iter().any(|a| &a.kind == kind) => {
                format!("none of its agents is a {kind} agent")
            }
            Only::Kind(kind) => format!("{kind} is not pinned"),
        });
    }
    Ok(out)
}

/// `pastor machine install`.
pub async fn cli(
    paths: &Paths,
    name: &str,
    only: Only,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<()> {
    let flock = crate::config::flock::Flock::load(&paths.flock_file())?;
    let Some(m) = flock.get(name) else {
        return Err(CliError::err(
            "unknown_machine",
            format!("no machine {name} in the flock"),
        ));
    };
    let config = crate::config::PastorConfig::load(&paths.config_file())?;
    let harnesses = Harnesses::load(paths)?;
    let parts = crate::machine_check::Config {
        defaults: &config.defaults,
        agents: &config.agents,
        models: &config.models,
        profiles: &config.profiles,
        harnesses: &harnesses,
        herdr_pin: None,
        head_address: None,
    };
    let agents = crate::machine_check::agents_on(m, &flock, &parts);
    let pins = flock.pins_for(m);
    let herdr_pin = config
        .artifacts
        .herdr
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let nothing = |why: String| CliError::err("nothing_pinned", format!("machine {name}: {why}"));
    // `--kind herdr` is herdr alone; `--agent` and any other kind are
    // harnesses; with neither, both.
    let herdr_only = matches!(&only, Only::Kind(k) if k == HERDR);
    let herdr_wanted = herdr_pin.is_some() && (herdr_only || only == Only::All);
    if herdr_only && herdr_pin.is_none() {
        return Err(nothing(
            "herdr is not pinned; pin it with `herdr` under [artifacts] in pastor.toml".into(),
        ));
    }
    let wants = if herdr_only {
        Vec::new()
    } else {
        match wanted(&agents, &pins, &harnesses, &only) {
            Ok(wants) => wants,
            Err(_) if herdr_wanted => Vec::new(),
            Err(why) => return Err(nothing(why)),
        }
    };
    let at_work = ListsAgents(Arc::new(Endpoint::from_machine(m, paths)));
    let herdr = herdr_wanted.then(|| HerdrWant {
        pin: herdr_pin.expect("checked above"),
        artifacts: &config.artifacts,
        live: &at_work,
    });
    let shell = run::MachineShell::new(m, paths).map_err(|e| CliError::err(e.code(), e))?;
    let report = install(
        paths,
        Arc::new(shell),
        name,
        &wants,
        herdr.as_ref(),
        &artifacts::CurlFetch,
        dry_run,
    )
    .await;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.text());
    }
    if !report.ok() {
        std::process::exit(1);
    }
    Ok(())
}

/// Install each of `wants` on the machine behind `shell` that is not at its
/// pin. With `dry_run`, say what would be done and change nothing, here or
/// there.
pub async fn install(
    paths: &Paths,
    shell: Arc<dyn Shell>,
    machine: &str,
    wants: &[Want<'_>],
    herdr: Option<&HerdrWant<'_>>,
    fetch: &dyn Fetch,
    dry_run: bool,
) -> Report {
    let mut job = Run {
        paths,
        shell,
        machine,
        fetch,
        dry_run,
        facts: None,
    };
    let mut rows = Vec::new();
    for w in wants {
        rows.push(job.one(w).await);
    }
    if let Some(h) = herdr {
        rows.push(job.herdr(h).await);
    }
    Report {
        machine: machine.to_string(),
        dry_run,
        rows,
    }
}

struct Run<'a> {
    paths: &'a Paths,
    shell: Arc<dyn Shell>,
    machine: &'a str,
    fetch: &'a dyn Fetch,
    dry_run: bool,
    /// The machine's facts, asked once, when a release needs them.
    facts: Option<Result<Facts, String>>,
}

/// What a probe found on the machine: the tools there, and what is at
/// `~/.local/bin/<binary>`.
struct Probe {
    tools: BTreeSet<String>,
    at: At,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    Nothing,
    Link,
    /// A file, or a link to a dir: not one pastor would replace.
    Other,
}

impl Probe {
    fn has(&self, tool: &str) -> bool {
        self.tools.contains(tool)
    }
}

/// The tools a probe looks for.
const TOOLS: [&str; 6] = ["npm", "curl", "bash", "sha256sum", "shasum", "tar"];

/// How a staged install's command sits in its dir.
enum Staged {
    /// The release file, as `<dir>/<binary>`.
    Release { sum: String },
    /// `npm install --prefix <dir>`, as `<dir>/node_modules/.bin/<binary>`.
    Npm { package: String },
}

impl Run<'_> {
    async fn one(&mut self, w: &Want<'_>) -> Row {
        match self.begin(w).await {
            Ok((row, bin)) => self.sources(row, w, &bin).await,
            Err(row) => *row,
        }
    }

    /// The row so far for `w`, and its command's name; `Err` is a row that
    /// is final: already at its pin, or no way to tell what is there.
    async fn begin(&self, w: &Want<'_>) -> Result<(Row, String), Box<Row>> {
        let m = w.manifest;
        let mut row = Row {
            kind: m.id.clone(),
            pin: w.pin.to_string(),
            before: None,
            after: None,
            source: None,
            action: Action::Failed,
            steps: Vec::new(),
            detail: None,
        };
        let (Some(command), Some(bin)) = (m.version_command(), m.binary.clone()) else {
            row.detail = Some(format!(
                "the {} manifest reads no version, so pastor cannot tell an install worked",
                m.id
            ));
            return Err(Box::new(row));
        };
        let before = match read_version(&*self.shell, m, command).await {
            Ok(v) => v,
            Err(why) => {
                row.detail = Some(format!("could not read its version: {why}"));
                return Err(Box::new(row));
            }
        };
        row.before.clone_from(&before);
        row.after.clone_from(&before);
        if before.as_deref().is_some_and(|v| at_pin(w.pin, v)) {
            row.action = Action::Kept;
            return Err(Box::new(row));
        }
        Ok((row, bin))
    }

    /// Install `w` from the first of its manifest's sources that fits.
    async fn sources(&mut self, mut row: Row, w: &Want<'_>, bin: &str) -> Row {
        let m = w.manifest;
        let bin = bin.to_string();
        let probe = match self.probe(m, &bin).await {
            Ok(p) => p,
            Err(why) => {
                row.detail = Some(why);
                return row;
            }
        };
        let link = format!("~/{LINK_DIR}/{bin}");
        let taken = format!("{link} is not a link pastor can replace; move it away first");
        let mut passed: Vec<String> = Vec::new();
        for source in &m.install {
            match source {
                Install::Release { url, sha256_url } => {
                    if !probe.has("sha256sum") && !probe.has("shasum") {
                        passed.push("release: no sha256sum or shasum to check it with".into());
                        continue;
                    }
                    if !probe.has("tar") {
                        passed.push("release: no tar to receive it with".into());
                        continue;
                    }
                    if probe.at == At::Other {
                        passed.push(format!("release: {taken}"));
                        continue;
                    }
                    let facts = match self.facts().await {
                        Ok(f) => f,
                        Err(why) => {
                            passed.push(format!("release: {why}"));
                            continue;
                        }
                    };
                    let fill = |u: &str| {
                        u.replace("{version}", w.pin)
                            .replace("{os}", &facts.os)
                            .replace("{arch}", &facts.arch)
                    };
                    let (url, sum_url) = (fill(url), fill(sha256_url));
                    row.source = Some("release".into());
                    row.steps = vec![
                        format!("fetch {url} into the head's cache and check it against {sum_url}"),
                        "send it to the machine and check its sha256 there".into(),
                        format!("put it in ~/{STAGE_ROOT}/{}/", m.id),
                        format!("check it says {}", w.pin),
                        format!("link {link} to it"),
                    ];
                    if self.dry_run {
                        row.action = Action::Installed;
                        return row;
                    }
                    let target = format!("{}-{}", facts.os, facts.arch);
                    let got = match Artifact::new(&format!("harness-{}", m.id), w.pin, &target) {
                        Ok(a) => {
                            // `latest` names another file each release.
                            if w.pin == harness::LATEST {
                                artifacts::forget(self.paths, &a);
                            }
                            artifacts::ensure_release(self.paths, &a, &url, &sum_url, self.fetch)
                                .await
                        }
                        Err(e) => Err(e.into()),
                    };
                    match got {
                        Ok((file, sum)) => {
                            return self
                                .staged(row, w, &bin, Staged::Release { sum }, Some(file.as_path()))
                                .await;
                        }
                        Err(e)
                            if artifacts::error_code(&e) == Some("artifact_checksum_mismatch") =>
                        {
                            row.action = Action::Refused;
                            row.detail = Some(format!("{e:#}"));
                            return row;
                        }
                        Err(e) => {
                            row.source = None;
                            row.steps.clear();
                            passed.push(format!("release: {e:#}"));
                        }
                    }
                }
                Install::Npm { package } => {
                    if !probe.has("npm") {
                        passed.push("npm: no npm on the machine's PATH".into());
                        continue;
                    }
                    if probe.at == At::Other {
                        passed.push(format!("npm: {taken}"));
                        continue;
                    }
                    let spec = format!("{package}@{}", w.pin);
                    row.source = Some("npm".into());
                    row.steps = vec![
                        format!(
                            "npm install {spec} into a new dir in ~/{STAGE_ROOT}/{}/",
                            m.id
                        ),
                        format!("check it says {}", w.pin),
                        format!("link {link} to it"),
                    ];
                    if self.dry_run {
                        row.action = Action::Installed;
                        return row;
                    }
                    return self
                        .staged(row, w, &bin, Staged::Npm { package: spec }, None)
                        .await;
                }
                Install::Script { url, shell, args } => {
                    let interpreter = match shell {
                        crate::harness_manifest::Shell::Sh => "sh",
                        crate::harness_manifest::Shell::Bash => "bash",
                    };
                    if !probe.has("curl") {
                        passed.push("script: no curl on the machine's PATH".into());
                        continue;
                    }
                    if interpreter == "bash" && !probe.has("bash") {
                        passed.push("script: no bash on the machine's PATH".into());
                        continue;
                    }
                    // The installer would write over the file, and only a
                    // link is saved to put back.
                    if probe.at == At::Other {
                        passed.push(format!("script: {taken}"));
                        continue;
                    }
                    let args: Vec<String> =
                        args.iter().map(|a| a.replace("{version}", w.pin)).collect();
                    row.source = Some("script".into());
                    row.steps = vec![
                        format!("save what {link} is"),
                        format!(
                            "run {url} with {interpreter}{}",
                            args.iter().map(|a| format!(" {a}")).collect::<String>()
                        ),
                        format!("check {bin} says {}, else put back what {link} was", w.pin),
                    ];
                    if self.dry_run {
                        row.action = Action::Installed;
                        return row;
                    }
                    return self.script(row, w, &bin, url, interpreter, &args).await;
                }
            }
        }
        row.detail = Some(if m.install.is_empty() {
            format!("{} has no install source", m.id)
        } else {
            format!("no install source fits: {}", passed.join("; "))
        });
        row
    }

    /// herdr at its pin: kept, or staged from the head's cache, read and
    /// linked. An update waits while tasks hold a pane on the machine.
    async fn herdr(&mut self, h: &HerdrWant<'_>) -> Row {
        let m = herdr_manifest();
        let w = Want {
            manifest: m,
            pin: h.pin,
        };
        let (mut row, bin) = match self.begin(&w).await {
            Ok(begun) => begun,
            Err(row) => return *row,
        };
        // Only an update waits: a machine with no herdr has no panes.
        if row.before.is_some() {
            match h.live.names().await {
                Ok(live) if live.is_empty() => {}
                Ok(live) => {
                    row.action = Action::Skipped;
                    row.detail = Some(format!(
                        "herdr lists {} on the machine; update it when {} closed",
                        live.join(", "),
                        if live.len() == 1 { "it is" } else { "they are" },
                    ));
                    return row;
                }
                Err(why) => {
                    row.detail = Some(format!(
                        "could not tell whether tasks are at work on the machine, so it was not updated: {why}"
                    ));
                    return row;
                }
            }
        }
        let probe = match self.probe(m, &bin).await {
            Ok(p) => p,
            Err(why) => {
                row.detail = Some(why);
                return row;
            }
        };
        let link = format!("~/{LINK_DIR}/{bin}");
        if probe.at == At::Other {
            row.detail = Some(format!(
                "{link} is not a link pastor can replace; move it away first"
            ));
            return row;
        }
        if !probe.has("sha256sum") && !probe.has("shasum") {
            row.detail = Some("no sha256sum or shasum on the machine to check it with".into());
            return row;
        }
        if !probe.has("tar") {
            row.detail = Some("no tar on the machine to receive it with".into());
            return row;
        }
        let target = match self.facts().await {
            Ok(facts) => artifacts::target_for(&facts, HERDR).map_err(|e| format!("{e:#}")),
            Err(why) => Err(why),
        };
        let (target, artifact) = match target.and_then(|t| {
            Artifact::new(HERDR, h.pin, &t)
                .map(|a| (t, a))
                .map_err(|e| e.to_string())
        }) {
            Ok(got) => got,
            Err(why) => {
                row.detail = Some(why);
                return row;
            }
        };
        row.source = Some("release".into());
        row.steps = vec![
            format!(
                "fetch herdr {} for {target} from its release index into the head's cache and check its sha256",
                h.pin
            ),
            "send it to the machine and check its sha256 there".into(),
            format!("put it in ~/{STAGE_ROOT}/{HERDR}/"),
            format!("check it says {}", h.pin),
            format!("link {link} to it"),
        ];
        if self.dry_run {
            row.action = Action::Installed;
            return row;
        }
        match artifacts::ensure(self.paths, h.artifacts, &artifact, self.fetch).await {
            Ok((file, sum)) => {
                self.staged(row, &w, &bin, Staged::Release { sum }, Some(file.as_path()))
                    .await
            }
            // No other source: what the index serves is the only herdr.
            Err(e) if artifacts::error_code(&e) == Some("artifact_checksum_mismatch") => {
                row.action = Action::Refused;
                row.detail = Some(format!("{e:#}"));
                row
            }
            Err(e) => {
                row.source = None;
                row.steps.clear();
                row.detail = Some(format!("{e:#}"));
                row
            }
        }
    }

    async fn facts(&mut self) -> Result<Facts, String> {
        if self.facts.is_none() {
            let got = run::facts(&*self.shell, self.machine)
                .await
                .map_err(|e| e.to_string());
            self.facts = Some(got);
        }
        self.facts.clone().expect("set above")
    }

    async fn probe(&self, m: &Manifest, bin: &str) -> Result<Probe, String> {
        let tools = TOOLS.join(" ");
        let script = format!(
            r#"{path}; printf '%s\n' {MARK}; for t in {tools}; do command -v "$t" >/dev/null 2>&1 && printf 'have %s\n' "$t"; done; f="$HOME/{LINK_DIR}/{bin}"; if [ -L "$f" ] && [ ! -d "$f" ]; then echo 'at link'; elif [ -L "$f" ] || [ -e "$f" ]; then echo 'at other'; else echo 'at nothing'; fi"#,
            path = path_line(m),
        );
        let ran = self.shell.run(script, Vec::new(), PROBE_TIME).await?;
        let stdout = String::from_utf8_lossy(&ran.stdout);
        let Some((_, answer)) = split_mark(&stdout) else {
            return Err(format!("the machine gave no answer{}", tail(&ran)));
        };
        let mut probe = Probe {
            tools: BTreeSet::new(),
            at: At::Nothing,
        };
        for line in answer.lines() {
            if let Some(t) = line.strip_prefix("have ") {
                probe.tools.insert(t.trim().to_string());
            } else if let Some(at) = line.strip_prefix("at ") {
                probe.at = match at.trim() {
                    "link" => At::Link,
                    "nothing" => At::Nothing,
                    _ => At::Other,
                };
            }
        }
        Ok(probe)
    }

    /// Install into a new dir under `STAGE_ROOT`, read what it says, and
    /// link it only when that is the pin; else remove the dir.
    async fn staged(
        &self,
        mut row: Row,
        w: &Want<'_>,
        bin: &str,
        how: Staged,
        file: Option<&std::path::Path>,
    ) -> Row {
        let m = w.manifest;
        let args = version_args(m);
        let root = format!("$HOME/{STAGE_ROOT}/{}", m.id);
        let (install, rel) = match &how {
            Staged::Release { sum } => (
                format!(
                    r#"got=$(sha256sum harness 2>/dev/null || shasum -a 256 harness 2>/dev/null); got=${{got%% *}}; if [ "$got" != {sum} ]; then printf 'its sha256 is %s here, not %s\n' "$got" {sum} >&2; rm -rf "$d"; exit 5; fi; cp harness "$d/{bin}" && chmod 755 "$d/{bin}" || {{ rm -rf "$d"; exit 3; }}"#,
                    sum = shell_quote(sum),
                ),
                bin.to_string(),
            ),
            Staged::Npm { package } => (
                format!(
                    r#"if ! npm install --prefix "$d" --no-audit --no-fund --loglevel=error {package} </dev/null >"$d/.npm.log" 2>&1; then tail -n 5 "$d/.npm.log" >&2; rm -rf "$d"; exit 4; fi; rm -f "$d/.npm.log""#,
                    package = shell_quote(package),
                ),
                format!("node_modules/.bin/{bin}"),
            ),
        };
        let script = format!(
            r#"{path}; mkdir -p "{root}" && d=$(mktemp -d "{root}/.new-XXXXXX") || exit 3; printf 'stage %s\n' "$d"; {install}; printf '%s\n' {MARK}; "$d/{rel}"{args} </dev/null 2>&1; true"#,
            path = path_line(m),
        );
        // A release travels in the run's tarball; npm needs no files.
        let (ran, dir) = match file {
            Some(file) => match self.send(file).await {
                Ok(dir) => {
                    let script = format!("cd {} || exit 3; {script}", shell_quote(dir.path()));
                    (
                        self.shell.run(script, Vec::new(), INSTALL_TIME).await,
                        Some(dir),
                    )
                }
                Err(why) => {
                    row.detail = Some(why);
                    return row;
                }
            },
            None => (self.shell.run(script, Vec::new(), INSTALL_TIME).await, None),
        };
        if let Some(dir) = dir {
            dir.remove().await;
        }
        let ran = match ran {
            Ok(ran) => ran,
            Err(why) => {
                row.detail = Some(format!("the install gave no answer: {why}"));
                return row;
            }
        };
        let stdout = String::from_utf8_lossy(&ran.stdout).to_string();
        if ran.code == Some(5) {
            row.action = Action::Refused;
            row.detail = Some(format!("checksum mismatch on the machine{}", tail(&ran)));
            return row;
        }
        let Some((before_mark, said)) = split_mark(&stdout).filter(|_| ran.code == Some(0)) else {
            row.detail = Some(format!("the install failed{}", tail(&ran)));
            return row;
        };
        let Some(stage) = before_mark
            .lines()
            .find_map(|l| l.strip_prefix("stage "))
            .map(str::to_string)
        else {
            row.detail = Some(format!("the install named no dir{}", tail(&ran)));
            return row;
        };
        let version = m.parse_version(said);
        let Some(version) = version.filter(|v| at_pin(w.pin, v) && path_part(v)) else {
            let _ = self
                .shell
                .run(
                    format!("rm -rf {}", shell_quote(&stage)),
                    Vec::new(),
                    PROBE_TIME,
                )
                .await;
            row.detail = Some(format!(
                "what it installed says {}, not {}; it was removed and nothing else changed",
                m.parse_version(said).unwrap_or_else(|| "no version".into()),
                w.pin
            ));
            return row;
        };
        let link = format!(
            r#"d={stage}; final="{root}/{version}"; if ! ( {refuse} ); then rm -rf "$d"; exit 6; fi; {save}; rm -rf "$final" && mv "$d" "$final" && rm -f "$f.pastor-new" && ln -s "$final/{rel}" "$f.pastor-new" && mv -f "$f.pastor-new" "$f""#,
            stage = shell_quote(&stage),
            refuse = refuse_taken(bin),
            save = save_link(bin),
        );
        match self.shell.run(link, Vec::new(), PROBE_TIME).await {
            Ok(ran) if ran.code == Some(0) => {}
            Ok(ran) if ran.code == Some(6) => {
                row.detail = Some(format!(
                    "{} is not a link pastor can replace; move it away first",
                    link_of(bin)
                ));
                return row;
            }
            Ok(ran) => {
                let undo = format!("rm -rf {}; {}", shell_quote(&stage), put_back(bin));
                let _ = self.shell.run(undo, Vec::new(), PROBE_TIME).await;
                row.detail = Some(format!("linking it failed{}", tail(&ran)));
                return row;
            }
            Err(why) => {
                row.detail = Some(format!("linking it gave no answer: {why}"));
                return row;
            }
        }
        let row = self.confirm(row, w, bin).await;
        let undo = if row.action == Action::Installed {
            drop_saved(bin)
        } else {
            put_back(bin)
        };
        let _ = self.shell.run(undo, Vec::new(), PROBE_TIME).await;
        row
    }

    /// Run the vendor's installer, keeping what `~/.local/bin/<binary>`
    /// was, and put that back when the version after is not the pin.
    async fn script(
        &self,
        mut row: Row,
        w: &Want<'_>,
        bin: &str,
        url: &str,
        interpreter: &str,
        args: &[String],
    ) -> Row {
        let m = w.manifest;
        let root = format!("$HOME/{STAGE_ROOT}/{}", m.id);
        let args: String = args
            .iter()
            .map(|a| format!(" {}", shell_quote(a)))
            .collect();
        let script = format!(
            r#"{path}; {refuse}; {save}; mkdir -p "{root}" || exit 3; s="{root}/.install.sh"; log="{root}/.install.log"; if ! curl -fsSL --proto =https -o "$s" --url {url} 2>"$log" || ! {interpreter} "$s"{args} </dev/null >"$log" 2>&1; then tail -n 5 "$log" >&2; rm -f "$s" "$log"; {back}; exit 4; fi; rm -f "$s" "$log"; printf '%s\n' {MARK}; {version}"#,
            path = path_line(m),
            refuse = refuse_taken(bin),
            save = save_link(bin),
            back = put_back(bin),
            url = shell_quote(url),
            version = m.version_command().unwrap_or_default(),
        );
        let ran = match self.shell.run(script, Vec::new(), INSTALL_TIME).await {
            Ok(ran) => ran,
            Err(why) => {
                row.detail = Some(format!("the installer gave no answer: {why}"));
                return row;
            }
        };
        if ran.code == Some(6) {
            row.detail = Some(format!(
                "{} is not a link pastor can replace; move it away first",
                link_of(bin)
            ));
            return row;
        }
        let stdout = String::from_utf8_lossy(&ran.stdout).to_string();
        let said = split_mark(&stdout)
            .filter(|_| ran.code == Some(0))
            .map(|(_, said)| said.to_string());
        let Some(said) = said else {
            row.detail = Some(format!(
                "the installer failed, and what {} was is back{}",
                link_of(bin),
                tail(&ran)
            ));
            return row;
        };
        let version = m.parse_version(&said).filter(|v| at_pin(w.pin, v));
        let Some(version) = version else {
            let _ = self.shell.run(put_back(bin), Vec::new(), PROBE_TIME).await;
            row.detail = Some(format!(
                "the installer brought {}, not {}; what {} was is back",
                m.parse_version(&said)
                    .unwrap_or_else(|| "no version".into()),
                w.pin,
                link_of(bin)
            ));
            return row;
        };
        let _ = self
            .shell
            .run(drop_saved(bin), Vec::new(), PROBE_TIME)
            .await;
        row.after = Some(version);
        row.action = Action::Installed;
        row
    }

    /// The version check after a staged install: what the machine's own
    /// PATH finds. Anything but `Installed` has the caller put the link
    /// back.
    async fn confirm(&self, mut row: Row, w: &Want<'_>, bin: &str) -> Row {
        let m = w.manifest;
        let command = m.version_command().unwrap_or_default();
        match read_version(&*self.shell, m, command).await {
            Ok(Some(v)) if at_pin(w.pin, &v) => {
                row.after = Some(v);
                row.action = Action::Installed;
            }
            Ok(v) => {
                row.after = v.clone();
                row.detail = Some(format!(
                    "{bin} on the machine's PATH says {} once {} links to {} {}: another {bin} comes first, so what {} was is back",
                    v.unwrap_or_else(|| "nothing".into()),
                    link_of(bin),
                    m.id,
                    w.pin,
                    link_of(bin),
                ));
            }
            Err(why) => {
                row.detail = Some(format!(
                    "could not read its version after, so what {} was is back: {why}",
                    link_of(bin)
                ));
            }
        }
        row
    }

    /// Send `file` to the machine as `harness` in a run directory.
    async fn send(&self, file: &std::path::Path) -> Result<run::Unpacked, String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = self.paths.state_dir.join("install");
        create_private_dir(&root).map_err(|e| format!("{e:#}"))?;
        let dir = root.join(format!(
            ".send-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let made = create_private_dir(&dir)
            .and_then(|()| {
                std::fs::copy(file, dir.join("harness"))?;
                Ok(())
            })
            .map_err(|e| format!("{e:#}"));
        let tarball = match made {
            Ok(()) => run::tar_dir(&dir).await,
            Err(e) => Err(e),
        };
        let _ = std::fs::remove_dir_all(&dir);
        run::unpack(self.shell.clone(), self.machine, tarball?)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Whether `version` is at `pin`: `latest` takes any.
fn at_pin(pin: &str, version: &str) -> bool {
    !version.is_empty() && harness::satisfies(pin, version)
}

/// A version that can name a dir.
fn path_part(v: &str) -> bool {
    !v.is_empty()
        && v != "."
        && v != ".."
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

fn link_of(bin: &str) -> String {
    format!("~/{LINK_DIR}/{bin}")
}

/// Shell that exits 6 when `~/.local/bin/<bin>` is a file or a link to a
/// dir: only a link to a file, or nothing, is pastor's to replace.
fn refuse_taken(bin: &str) -> String {
    format!(
        r#"f="$HOME/{LINK_DIR}/{bin}"; if {{ [ -L "$f" ] && [ -d "$f" ]; }} || {{ [ ! -L "$f" ] && [ -e "$f" ]; }}; then echo "$f is not a link pastor can replace" >&2; exit 6; fi"#
    )
}

/// Shell that saves what `~/.local/bin/<bin>` is beside it, as
/// `.pastor-before`, or marks with `.pastor-none` that nothing was there,
/// so `put_back` can undo an install either way. Exits 3 when it cannot.
fn save_link(bin: &str) -> String {
    format!(
        r#"f="$HOME/{LINK_DIR}/{bin}"; b="$f.pastor-before"; n="$f.pastor-none"; rm -rf "$b" "$n"; mkdir -p "$HOME/{LINK_DIR}" || exit 3; if [ -L "$f" ] || [ -e "$f" ]; then cp -P "$f" "$b" || exit 3; else : > "$n" || exit 3; fi"#
    )
}

/// Shell that puts back what `save_link` saved: the old link, or nothing
/// when nothing was there.
fn put_back(bin: &str) -> String {
    format!(
        r#"f="$HOME/{LINK_DIR}/{bin}"; b="$f.pastor-before"; n="$f.pastor-none"; if [ -L "$b" ] || [ -e "$b" ]; then rm -rf "$f"; mv -f "$b" "$f"; elif [ -e "$n" ]; then rm -rf "$f" "$n"; fi"#
    )
}

/// Shell that drops what `save_link` saved, once the install holds.
fn drop_saved(bin: &str) -> String {
    format!(r#"rm -rf "$HOME/{LINK_DIR}/{bin}.pastor-before" "$HOME/{LINK_DIR}/{bin}.pastor-none""#)
}

/// `PATH` with `~/.local/bin` and the manifest's `bin_dirs` first, as the
/// version command has it.
fn path_line(m: &Manifest) -> String {
    let mut dirs = vec![format!("\"$HOME/{LINK_DIR}\":")];
    for d in &m.bin_dirs {
        let d = format!("\"$HOME/{}\":", d.trim_start_matches("~/"));
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    format!("PATH={}\"$PATH\"; export PATH", dirs.concat())
}

/// The manifest's version arguments, each quoted, with a leading space.
fn version_args(m: &Manifest) -> String {
    m.version
        .as_ref()
        .map(|v| {
            v.args
                .iter()
                .map(|a| format!(" {}", shell_quote(a)))
                .collect()
        })
        .unwrap_or_default()
}

/// The version the machine's PATH finds: `None` when the harness is not
/// there, `Some("")` when it printed no version.
async fn read_version(
    shell: &dyn Shell,
    m: &Manifest,
    command: String,
) -> Result<Option<String>, String> {
    let ran = shell.run(command, Vec::new(), PROBE_TIME).await?;
    let text = String::from_utf8_lossy(&ran.stdout).to_string();
    let last = text.lines().rev().find(|l| !l.trim().is_empty());
    Ok(match last.map(str::trim) {
        None | Some("none") => None,
        Some(_) => Some(m.parse_version(&text).unwrap_or_default()),
    })
}

/// What comes before and after the `MARK` line.
fn split_mark(stdout: &str) -> Option<(&str, &str)> {
    let line = format!("{MARK}\n");
    stdout
        .find(&line)
        .map(|i| (&stdout[..i], &stdout[i + line.len()..]))
}

/// The last lines the machine wrote to stderr, after a `: `, or nothing.
fn tail(ran: &Ran) -> String {
    let err = String::from_utf8_lossy(&ran.stderr);
    let lines: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
    let last = &lines[lines.len().saturating_sub(3)..];
    if last.is_empty() {
        String::new()
    } else {
        format!(": {}", last.join(" / "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provision::run::testing::Fake;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A machine: a home, and a dir of stand-in tools on its PATH before
    /// the system's.
    struct Machine {
        tmp: tempfile::TempDir,
        paths: Paths,
        home: PathBuf,
        shell: Arc<dyn Shell>,
    }

    fn script(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// npm that installs a `<bin>` saying the version it was asked for, or
    /// what `~/npm-gives` holds.
    const NPM: &str = r#"#!/bin/sh
prefix=; spec=
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix=$2; shift 2 ;;
    install|--*) shift ;;
    *) spec=$1; shift ;;
  esac
done
v=${spec##*@}
[ -f "$HOME/npm-gives" ] && v=$(cat "$HOME/npm-gives")
mkdir -p "$prefix/node_modules/.bin"
printf '#!/bin/sh\necho "%s (Claude Code)"\n' "$v" > "$prefix/node_modules/.bin/claude"
chmod 755 "$prefix/node_modules/.bin/claude"
echo "added 1 package"
"#;

    impl Machine {
        fn new() -> Machine {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            let tools = tmp.path().join("tools");
            script(&tools.join("npm"), NPM);
            // No test reaches the network: a test that wants a download
            // puts its own curl here.
            script(
                &tools.join("curl"),
                "#!/bin/sh\necho 'curl: no network in tests' >&2\nexit 7\n",
            );
            let paths = Paths::new(tmp.path().join("cfg"), tmp.path().join("state"));
            let path = format!("{}:/usr/bin:/bin", tools.display());
            let shell: Arc<dyn Shell> = Arc::new(Fake::new(&home, Some(path)));
            Machine {
                tmp,
                paths,
                home,
                shell,
            }
        }

        /// `~/.local/bin/<bin>` a link to a `<bin>` that says `says`.
        fn has(&self, bin: &str, says: &str) -> PathBuf {
            let old = self.home.join("old").join(bin);
            script(&old, &format!("#!/bin/sh\necho '{says}'\n"));
            let link = self.home.join(LINK_DIR).join(bin);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&old, &link).unwrap();
            old
        }

        /// A curl that hands over `installer`, wherever it is asked for.
        fn serves(&self, installer: &str) -> Manifest {
            let path = self.tmp.path().join("installer.sh");
            script(&path, installer);
            script(
                &self.tmp.path().join("tools/curl"),
                &format!(
                    "#!/bin/sh\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=$2; shift; done\ncp {} \"$out\"\n",
                    path.display()
                ),
            );
            Manifest::parse(
                "id = \"tool\"\nbinary = \"tool\"\nbin_dirs = [\"~/.local/bin\"]\n\
                 [version]\nargs = [\"--version\"]\n\
                 [[install]]\nsource = \"script\"\nurl = \"https://x.example/install.sh\"\n\
                 args = [\"{version}\"]\n[paths]\nhome = \"~/.tool\"\n",
            )
            .unwrap()
        }

        fn link(&self, bin: &str) -> PathBuf {
            std::fs::read_link(self.home.join(LINK_DIR).join(bin)).unwrap()
        }

        async fn install(&self, m: &Manifest, pin: &str, fetch: &dyn Fetch, dry: bool) -> Row {
            let wants = [Want { manifest: m, pin }];
            let report = install(
                &self.paths,
                self.shell.clone(),
                "pi-3",
                &wants,
                None,
                fetch,
                dry,
            )
            .await;
            report.rows.into_iter().next().unwrap()
        }
    }

    /// Serves a release file and its sum; `sum` is what the sum file says.
    struct Release {
        file: Vec<u8>,
        sum: String,
    }

    impl Fetch for Release {
        fn get(&self, url: &str, to: &Path) -> anyhow::Result<()> {
            if url.ends_with(".sha256") {
                std::fs::write(to, format!("{}  tool\n", self.sum))?;
            } else {
                std::fs::write(to, &self.file)?;
            }
            Ok(())
        }
    }

    struct NoFetch;

    impl Fetch for NoFetch {
        fn get(&self, url: &str, _: &Path) -> anyhow::Result<()> {
            anyhow::bail!("no network here: {url}")
        }
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A vendor's installer that puts a `tool` saying its first argument,
    /// or what `~/gives` holds, at `~/.local/bin/tool`.
    const INSTALLER: &str = "#!/bin/sh\nv=$1\n[ -f \"$HOME/gives\" ] && v=$(cat \"$HOME/gives\")\n\
         mkdir -p \"$HOME/.local/bin\"\nrm -f \"$HOME/.local/bin/tool\"\n\
         printf '#!/bin/sh\\necho \"tool %s\"\\n' \"$v\" > \"$HOME/.local/bin/tool\"\n\
         chmod 755 \"$HOME/.local/bin/tool\"\n";

    fn claude() -> &'static Manifest {
        Harnesses::builtin().get("claude").unwrap()
    }

    /// A tool released as one file with a published sum, then on npm.
    fn tool() -> Manifest {
        Manifest::parse(
            "id = \"tool\"\nbinary = \"tool\"\nbin_dirs = [\"~/.local/bin\"]\n\
             [version]\nargs = [\"--version\"]\n\
             [[install]]\nsource = \"release\"\n\
             url = \"https://x.example/{version}/tool-{os}-{arch}\"\n\
             sha256_url = \"https://x.example/{version}/tool-{os}-{arch}.sha256\"\n\
             [[install]]\nsource = \"npm\"\npackage = \"tool\"\n\
             [paths]\nhome = \"~/.tool\"\n",
        )
        .unwrap()
    }

    /// From the version before to the pin, by npm into a dir of pastor's,
    /// linked from `~/.local/bin`; asked again, it is kept.
    #[tokio::test]
    async fn a_machine_moves_from_another_version_to_the_pin() {
        let m = Machine::new();
        m.has("claude", "2.1.309 (Claude Code)");
        let row = m.install(claude(), "2.1.310", &NoFetch, false).await;
        assert_eq!(row.action, Action::Installed, "{row:?}");
        assert_eq!(row.source.as_deref(), Some("npm"));
        assert_eq!(row.before.as_deref(), Some("2.1.309"));
        assert_eq!(row.after.as_deref(), Some("2.1.310"));
        assert_eq!(
            m.link("claude"),
            m.home
                .join(STAGE_ROOT)
                .join("claude/2.1.310/node_modules/.bin/claude")
        );
        let again = m.install(claude(), "2.1.310", &NoFetch, false).await;
        assert_eq!(again.action, Action::Kept, "{again:?}");
        // A prefix pin takes what is there.
        let prefix = m.install(claude(), "2.1", &NoFetch, false).await;
        assert_eq!(prefix.action, Action::Kept, "{prefix:?}");
    }

    /// A dry run says which source and steps it would take, and changes
    /// nothing on the machine.
    #[tokio::test]
    async fn a_dry_run_lists_the_steps_and_changes_nothing() {
        let m = Machine::new();
        let old = m.has("claude", "2.1.309 (Claude Code)");
        let row = m.install(claude(), "2.1.310", &NoFetch, true).await;
        assert_eq!(row.action, Action::Installed, "{row:?}");
        assert_eq!(row.source.as_deref(), Some("npm"));
        assert_eq!(row.after.as_deref(), Some("2.1.309"), "nothing moved");
        assert!(
            row.steps[0].starts_with("npm install @anthropic-ai/claude-code@2.1.310"),
            "{row:?}"
        );
        assert_eq!(m.link("claude"), old);
        assert!(!m.home.join(".local/share/pastor").exists());
        let report = Report {
            machine: "pi-3".into(),
            dry_run: true,
            rows: vec![row],
        };
        let text = report.text();
        assert!(
            text.contains("would install  claude 2.1.309 -> 2.1.310 from npm\n"),
            "{text}"
        );
        assert!(text.contains("      - npm install"), "{text}");
    }

    /// An install that brings another version than the pin is taken out
    /// again, and the link stays where it was.
    #[tokio::test]
    async fn an_install_of_the_wrong_version_leaves_the_old_one() {
        let m = Machine::new();
        let old = m.has("claude", "2.1.309 (Claude Code)");
        std::fs::write(m.home.join("npm-gives"), "2.1.300").unwrap();
        let row = m.install(claude(), "2.1.310", &NoFetch, false).await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .contains("says 2.1.300, not 2.1.310"),
            "{row:?}"
        );
        assert_eq!(m.link("claude"), old);
        let staged = m.home.join(STAGE_ROOT).join("claude");
        assert_eq!(std::fs::read_dir(&staged).unwrap().count(), 0, "removed");
    }

    /// A release that does not match its published sum is refused on the
    /// head, and nothing falls back to the next source; one that matches
    /// is checked again on the machine and linked.
    #[tokio::test]
    async fn a_release_with_the_wrong_sum_is_refused() {
        let m = Machine::new();
        let tool = tool();
        let file = b"#!/bin/sh\necho 'tool 1.2.3'\n".to_vec();
        let bad = Release {
            file: file.clone(),
            sum: sha(b"something else"),
        };
        let row = m.install(&tool, "1.2.3", &bad, false).await;
        assert_eq!(row.action, Action::Refused, "{row:?}");
        assert!(
            row.detail.as_deref().unwrap().contains("checksum mismatch"),
            "{row:?}"
        );
        assert!(!m.home.join(LINK_DIR).join("tool").exists());
        assert!(
            !m.home.join(STAGE_ROOT).join("tool").exists(),
            "npm was not tried"
        );

        let good = Release {
            sum: sha(&file),
            file,
        };
        let row = m.install(&tool, "1.2.3", &good, false).await;
        assert_eq!(row.action, Action::Installed, "{row:?}");
        assert_eq!(row.source.as_deref(), Some("release"));
        assert_eq!(row.after.as_deref(), Some("1.2.3"));
        assert_eq!(
            m.link("tool"),
            m.home.join(STAGE_ROOT).join("tool/1.2.3/tool")
        );
    }

    /// A vendor's installer runs with the pin; when what it leaves is not
    /// the pin, what `~/.local/bin/<binary>` was is put back.
    #[tokio::test]
    async fn a_vendor_installer_that_misses_the_pin_is_undone() {
        let m = Machine::new();
        let tool = m.serves(INSTALLER);
        let old = m.has("tool", "tool 1.2.3");
        std::fs::write(m.home.join("gives"), "1.0.0").unwrap();
        let row = m.install(&tool, "1.2.4", &NoFetch, false).await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        assert_eq!(m.link("tool"), old, "put back");
        assert!(!m.home.join(".local/bin/tool.pastor-before").exists());

        std::fs::remove_file(m.home.join("gives")).unwrap();
        let row = m.install(&tool, "1.2.4", &NoFetch, false).await;
        assert_eq!(row.action, Action::Installed, "{row:?}");
        assert_eq!(row.source.as_deref(), Some("script"));
        assert_eq!(row.after.as_deref(), Some("1.2.4"));
        assert!(!m.home.join(".local/bin/tool.pastor-before").exists());
    }

    /// When nothing was at `~/.local/bin/<binary>` before, an installer
    /// that misses the pin or fails leaves nothing there either.
    #[tokio::test]
    async fn a_vendor_install_undone_with_nothing_before_leaves_nothing() {
        let m = Machine::new();
        let tool = m.serves(INSTALLER);
        std::fs::write(m.home.join("gives"), "1.0.0").unwrap();
        let row = m.install(&tool, "1.2.4", &NoFetch, false).await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        let link = m.home.join(LINK_DIR).join("tool");
        assert!(!link.exists() && !link.is_symlink(), "{row:?}");
        assert!(!m.home.join(".local/bin/tool.pastor-none").exists());

        let fails = m.serves(&format!("{INSTALLER}exit 1\n"));
        std::fs::remove_file(m.home.join("gives")).unwrap();
        let row = m.install(&fails, "1.2.4", &NoFetch, false).await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        assert!(!link.exists() && !link.is_symlink(), "{row:?}");
        assert!(!m.home.join(".local/bin/tool.pastor-none").exists());
    }

    /// A file at `~/.local/bin/<binary>` that is not a link is the user's:
    /// no source replaces it, the vendor's installer included.
    #[tokio::test]
    async fn a_file_where_the_link_goes_is_left_alone() {
        let m = Machine::new();
        let file = m.home.join(LINK_DIR).join("claude");
        script(&file, "#!/bin/sh\necho '2.1.309 (Claude Code)'\n");
        let before = std::fs::read(&file).unwrap();
        let row = m.install(claude(), "2.1.310", &NoFetch, false).await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        let detail = row.detail.as_deref().unwrap();
        assert!(
            detail.contains(
                "npm: ~/.local/bin/claude is not a link pastor can replace; move it away first"
            ),
            "{row:?}"
        );
        assert!(
            detail.contains("script: ~/.local/bin/claude is not a link"),
            "{row:?}"
        );
        assert!(!file.is_symlink());
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert!(!m.home.join(STAGE_ROOT).exists(), "nothing was staged");
    }

    /// The machine checks a release's sum again: one that comes out
    /// another there is refused, and nothing is staged or linked.
    #[tokio::test]
    async fn a_release_that_sums_another_on_the_machine_is_refused() {
        let m = Machine::new();
        script(
            &m.tmp.path().join("tools/sha256sum"),
            &format!("#!/bin/sh\necho \"{}  $1\"\n", sha(b"something else")),
        );
        let file = b"#!/bin/sh\necho 'tool 1.2.3'\n".to_vec();
        let release = Release {
            sum: sha(&file),
            file,
        };
        let row = m.install(&tool(), "1.2.3", &release, false).await;
        assert_eq!(row.action, Action::Refused, "{row:?}");
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .contains("checksum mismatch on the machine"),
            "{row:?}"
        );
        assert!(!m.home.join(LINK_DIR).join("tool").is_symlink());
        let staged = m.home.join(STAGE_ROOT).join("tool");
        assert_eq!(std::fs::read_dir(&staged).unwrap().count(), 0, "removed");
    }

    /// When another copy comes first on the PATH, the install fails and
    /// the link is put back as it was.
    #[tokio::test]
    async fn a_shadowed_install_puts_the_link_back() {
        let m = Machine::new();
        script(
            &m.home.join("shadow/tool"),
            "#!/bin/sh\necho 'tool 1.0.0'\n",
        );
        let old = m.has("tool", "tool 1.1.0");
        let tool = Manifest::parse(
            "id = \"tool\"\nbinary = \"tool\"\nbin_dirs = [\"~/shadow\", \"~/.local/bin\"]\n\
             [version]\nargs = [\"--version\"]\n\
             [[install]]\nsource = \"release\"\n\
             url = \"https://x.example/{version}/tool\"\n\
             sha256_url = \"https://x.example/{version}/tool.sha256\"\n\
             [paths]\nhome = \"~/.tool\"\n",
        )
        .unwrap();
        let file = b"#!/bin/sh\necho 'tool 1.2.3'\n".to_vec();
        let release = Release {
            sum: sha(&file),
            file,
        };
        let row = m.install(&tool, "1.2.3", &release, false).await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        assert!(
            row.detail.as_deref().unwrap().contains("comes first"),
            "{row:?}"
        );
        assert_eq!(m.link("tool"), old, "put back");
        assert!(!m.home.join(".local/bin/tool.pastor-before").exists());
    }

    /// A `latest` release is fetched again each time, not served from
    /// the cache under that name.
    #[tokio::test]
    async fn a_latest_release_is_fetched_again() {
        let m = Machine::new();
        let release = |v: &str| {
            let file = format!("#!/bin/sh\necho 'tool {v}'\n").into_bytes();
            Release {
                sum: sha(&file),
                file,
            }
        };
        let row = m.install(&tool(), "latest", &release("1.2.3"), false).await;
        assert_eq!(row.after.as_deref(), Some("1.2.3"), "{row:?}");
        std::fs::remove_file(m.home.join(LINK_DIR).join("tool")).unwrap();
        let row = m.install(&tool(), "latest", &release("1.2.4"), false).await;
        assert_eq!(row.action, Action::Installed, "{row:?}");
        assert_eq!(row.after.as_deref(), Some("1.2.4"), "{row:?}");
    }

    /// A harness with no install source says so, and changes nothing.
    #[tokio::test]
    async fn a_harness_with_no_source_says_so() {
        let m = Machine::new();
        let agy = Harnesses::builtin().get("agy").unwrap();
        let row = m.install(agy, "1.0.0", &NoFetch, false).await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        assert_eq!(row.detail.as_deref(), Some("agy has no install source"));
    }

    /// herdr's release index, and its one release file for every target.
    struct HerdrIndex {
        version: String,
        file: Vec<u8>,
        sum: String,
    }

    const INDEX_URL: &str = "https://herdr.example/latest.json";

    impl Fetch for HerdrIndex {
        fn get(&self, url: &str, to: &Path) -> anyhow::Result<()> {
            if url == INDEX_URL {
                let targets = [
                    "linux-x86_64",
                    "linux-aarch64",
                    "macos-x86_64",
                    "macos-aarch64",
                ];
                let index = serde_json::json!({"releases": {&self.version: {
                    "assets": targets.map(|t| (t, "https://herdr.example/herdr")).into_iter().collect::<std::collections::BTreeMap<_, _>>(),
                    "sha256": targets.map(|t| (t, self.sum.clone())).into_iter().collect::<std::collections::BTreeMap<_, _>>(),
                }}});
                std::fs::write(to, index.to_string())?;
            } else if url == "https://herdr.example/herdr" {
                std::fs::write(to, &self.file)?;
            } else {
                anyhow::bail!("no network here: {url}");
            }
            Ok(())
        }
    }

    fn herdr_release(version: &str) -> HerdrIndex {
        let file = format!("#!/bin/sh\necho 'herdr {version}'\n").into_bytes();
        HerdrIndex {
            version: "0.9.5".into(),
            sum: sha(&file),
            file,
        }
    }

    /// Who herdr lists on the machine, or why that could not be told.
    struct Listed(Result<Vec<String>, String>);

    impl AtWork for Listed {
        fn names(&self) -> AtWorkFuture<'_> {
            Box::pin(async { self.0.clone() })
        }
    }

    impl Machine {
        /// `herdr` at `pin`, with `live` named by herdr as at work.
        async fn herdr(
            &self,
            pin: &str,
            live: Result<Vec<String>, String>,
            fetch: &dyn Fetch,
            dry: bool,
        ) -> Row {
            let cfg = ArtifactsConfig {
                herdr: Some(pin.into()),
                herdr_index: Some(INDEX_URL.into()),
            };
            let live = Listed(live);
            let herdr = HerdrWant {
                pin,
                artifacts: &cfg,
                live: &live,
            };
            let report = install(
                &self.paths,
                self.shell.clone(),
                "pi-3",
                &[],
                Some(&herdr),
                fetch,
                dry,
            )
            .await;
            report.rows.into_iter().next().unwrap()
        }

        /// The staged herdr dirs, and the files left in the head's cache.
        fn staged_herdr(&self) -> Vec<String> {
            let dir = self.home.join(STAGE_ROOT).join(HERDR);
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .map(|d| {
                    d.map(|e| e.unwrap().file_name().to_string_lossy().into())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    /// From another version to the pin: the head's cache gets the release,
    /// the machine checks it, reads what it says and links it; asked again,
    /// it is kept without a fetch.
    #[tokio::test]
    async fn herdr_moves_to_its_pin() {
        let m = Machine::new();
        let old = m.has("herdr", "herdr 0.9.4");
        let row = m
            .herdr("0.9.5", Ok(vec![]), &herdr_release("0.9.5"), false)
            .await;
        assert_eq!(row.kind, "herdr");
        assert_eq!(row.action, Action::Installed, "{row:?}");
        assert_eq!(row.before.as_deref(), Some("0.9.4"));
        assert_eq!(row.after.as_deref(), Some("0.9.5"));
        assert_ne!(m.link("herdr"), old);
        assert_eq!(
            m.link("herdr"),
            m.home.join(STAGE_ROOT).join("herdr/0.9.5/herdr")
        );
        assert!(old.exists(), "the old copy is not deleted");
        let again = m.herdr("0.9.5", Ok(vec![]), &NoFetch, false).await;
        assert_eq!(again.action, Action::Kept, "{again:?}");
    }

    /// A herdr that is not there is installed whatever tasks are at work:
    /// only an update waits.
    #[tokio::test]
    async fn herdr_is_installed_where_there_is_none_even_with_tasks_at_work() {
        let m = Machine::new();
        let row = m
            .herdr(
                "0.9.5",
                Ok(vec!["t-1".into()]),
                &herdr_release("0.9.5"),
                false,
            )
            .await;
        assert_eq!(row.action, Action::Installed, "{row:?}");
        assert_eq!(row.before, None);
        assert_eq!(row.after.as_deref(), Some("0.9.5"));
    }

    /// An update does not go on while tasks are at work on the machine: it
    /// is skipped and says which, and nothing is fetched, staged or linked.
    #[tokio::test]
    async fn herdr_is_not_updated_while_tasks_are_at_work() {
        let m = Machine::new();
        let old = m.has("herdr", "herdr 0.9.4");
        let live = Ok(vec!["t-3".to_string(), "t-5".to_string()]);
        let row = m.herdr("0.9.5", live.clone(), &NoFetch, false).await;
        assert_eq!(row.action, Action::Skipped, "{row:?}");
        let detail = row.detail.as_deref().unwrap();
        assert!(detail.contains("t-3, t-5"), "{detail}");
        assert!(detail.contains("when they are closed"), "{detail}");
        assert_eq!(row.after.as_deref(), Some("0.9.4"));
        assert_eq!(m.link("herdr"), old);
        assert!(m.staged_herdr().is_empty());
        assert!(!m.paths.state_dir.join("artifacts").exists(), "no fetch");
        // A dry run says the same, and the report is not ok.
        let dry = m.herdr("0.9.5", live, &NoFetch, true).await;
        assert_eq!(dry.action, Action::Skipped, "{dry:?}");
        let report = Report {
            machine: "pi-3".into(),
            dry_run: false,
            rows: vec![row],
        };
        assert!(!report.ok());
        assert!(report.text().contains("skipped"), "{}", report.text());
    }

    /// When it cannot be told whether tasks are at work, an update waits:
    /// herdr stays where it was.
    #[tokio::test]
    async fn herdr_is_not_updated_when_the_tasks_cannot_be_read() {
        let m = Machine::new();
        let old = m.has("herdr", "herdr 0.9.4");
        let row = m
            .herdr(
                "0.9.5",
                Err("database is locked".into()),
                &herdr_release("0.9.5"),
                false,
            )
            .await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .contains("database is locked"),
            "{row:?}"
        );
        assert_eq!(m.link("herdr"), old);
        assert!(m.staged_herdr().is_empty());
    }

    /// A release that says another version than the pin is taken out
    /// again, and the link stays where it was.
    #[tokio::test]
    async fn herdr_that_says_another_version_leaves_the_old_one() {
        let m = Machine::new();
        let old = m.has("herdr", "herdr 0.9.4");
        let row = m
            .herdr("0.9.5", Ok(vec![]), &herdr_release("0.9.3"), false)
            .await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .contains("says 0.9.3, not 0.9.5"),
            "{row:?}"
        );
        assert_eq!(m.link("herdr"), old);
        assert!(m.staged_herdr().is_empty(), "{:?}", m.staged_herdr());
    }

    /// A release that does not match the sum the index publishes is
    /// refused on the head, caches nothing and falls back to nothing.
    #[tokio::test]
    async fn herdr_with_the_wrong_sum_is_refused() {
        let m = Machine::new();
        let old = m.has("herdr", "herdr 0.9.4");
        let mut bad = herdr_release("0.9.5");
        bad.sum = sha(b"something else");
        let row = m.herdr("0.9.5", Ok(vec![]), &bad, false).await;
        assert_eq!(row.action, Action::Refused, "{row:?}");
        assert!(
            row.detail.as_deref().unwrap().contains("checksum mismatch"),
            "{row:?}"
        );
        assert_eq!(m.link("herdr"), old);
        assert!(m.staged_herdr().is_empty());
        assert!(!m.paths.state_dir.join("artifacts/herdr/0.9.5").exists());
    }

    /// The machine checks the sum again: a copy that sums another there is
    /// refused and nothing is staged or linked.
    #[tokio::test]
    async fn herdr_that_sums_another_on_the_machine_is_refused() {
        let m = Machine::new();
        let old = m.has("herdr", "herdr 0.9.4");
        script(
            &m.tmp.path().join("tools/sha256sum"),
            &format!("#!/bin/sh\necho \"{}  $1\"\n", sha(b"something else")),
        );
        let row = m
            .herdr("0.9.5", Ok(vec![]), &herdr_release("0.9.5"), false)
            .await;
        assert_eq!(row.action, Action::Refused, "{row:?}");
        assert_eq!(m.link("herdr"), old);
        assert!(m.staged_herdr().is_empty());
    }

    /// A herdr that is a file, not a link, is the user's: it is never
    /// replaced, and nothing is fetched for it.
    #[tokio::test]
    async fn a_real_herdr_file_is_left_alone() {
        let m = Machine::new();
        let file = m.home.join(LINK_DIR).join("herdr");
        script(&file, "#!/bin/sh\necho 'herdr 0.9.4'\n");
        let before = std::fs::read(&file).unwrap();
        let row = m.herdr("0.9.5", Ok(vec![]), &NoFetch, false).await;
        assert_eq!(row.action, Action::Failed, "{row:?}");
        assert!(
            row.detail.as_deref().unwrap().contains(
                "~/.local/bin/herdr is not a link pastor can replace; move it away first"
            ),
            "{row:?}"
        );
        assert!(!file.is_symlink());
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert!(m.staged_herdr().is_empty());
    }

    /// A dry run lists the steps and changes nothing, on the head or on the
    /// machine.
    #[tokio::test]
    async fn a_herdr_dry_run_changes_nothing() {
        let m = Machine::new();
        let old = m.has("herdr", "herdr 0.9.4");
        let row = m.herdr("0.9.5", Ok(vec![]), &NoFetch, true).await;
        assert_eq!(row.action, Action::Installed, "{row:?}");
        assert_eq!(row.source.as_deref(), Some("release"));
        assert!(
            row.steps[0].starts_with("fetch herdr 0.9.5 for "),
            "{row:?}"
        );
        assert_eq!(m.link("herdr"), old);
        assert!(!m.home.join(".local/share/pastor").exists());
        assert!(!m.paths.state_dir.join("artifacts").exists());
    }

    /// herdr's version is read from what `herdr --version` prints.
    #[test]
    fn herdr_says_its_version_in_its_first_line() {
        assert_eq!(
            herdr_version_from("herdr 0.9.1\n").as_deref(),
            Some("0.9.1")
        );
        assert_eq!(herdr_version_from("none"), None);
        assert!(herdr_version_command().contains("herdr --version"));
    }

    /// What is at work is what herdr lists, named as a task's agent: every
    /// `t-<id>` there is, whatever any task row says, an orphan with no row
    /// included; a pane a person named, or a name that is not canonical, is
    /// not. A herdr that cannot be asked says why.
    #[cfg(feature = "fake-herdr")]
    #[tokio::test]
    async fn what_herdr_lists_as_a_tasks_agent_is_at_work() {
        let fake = crate::herdr::fake::FakeHerdr::new();
        for name in ["t-12", "t-3", "mine", "7", "t-01"] {
            let created = fake
                .workspace_create(None, name, &Default::default())
                .await
                .unwrap();
            fake.agent_start(name, "claude", &created.root_pane.pane_id, &[])
                .await
                .unwrap();
        }
        let got = ListsAgents(Arc::new(fake)).names().await;
        assert_eq!(got, Ok(vec!["t-12".to_string(), "t-3".to_string()]));

        struct Down;
        impl crate::herdr::transport::Connector for Down {
            fn connect(&self) -> crate::herdr::transport::ConnectFuture<'_> {
                Box::pin(async {
                    Err(crate::herdr::transport::ConnectError {
                        message: "no route to the machine".into(),
                    })
                })
            }
            fn describe(&self) -> String {
                "down".into()
            }
        }
        let err = ListsAgents(Arc::new(Down)).names().await.unwrap_err();
        assert!(err.contains("no route to the machine"), "{err}");
    }

    /// Only the pinned kinds the machine's agents run, each once, narrowed
    /// by `--agent` or `--kind`; what matches nothing says why.
    #[test]
    fn the_pinned_kinds_of_the_machines_agents_are_wanted() {
        let agent = |name: &str, kind: &str| run::AgentEnv {
            name: name.into(),
            kind: kind.into(),
            env: Default::default(),
        };
        let agents = [
            agent("claude-main", "claude"),
            agent("claude-base", "claude"),
            agent("agy", "agy"),
        ];
        let pins: harness::Pins = [
            ("claude".to_string(), "2.1.310".to_string()),
            ("codex".to_string(), "1".to_string()),
        ]
        .into();
        let all = Harnesses::builtin();
        let kinds = |only: Only| -> Result<Vec<String>, String> {
            wanted(&agents, &pins, all, &only)
                .map(|w| w.iter().map(|w| w.manifest.id.clone()).collect())
        };
        assert_eq!(kinds(Only::All).unwrap(), ["claude"]);
        assert_eq!(
            kinds(Only::Agent("claude-base".into())).unwrap(),
            ["claude"]
        );
        assert_eq!(
            kinds(Only::Agent("agy".into())).unwrap_err(),
            "the harness agent agy runs is not pinned"
        );
        assert_eq!(
            kinds(Only::Agent("nobody".into())).unwrap_err(),
            "no agent nobody can land on it"
        );
        assert_eq!(
            kinds(Only::Kind("codex".into())).unwrap_err(),
            "none of its agents is a codex agent"
        );
    }
}
