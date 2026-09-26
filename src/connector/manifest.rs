//! `pastor-connector.toml`: the file's shape (`ManifestFile`) and what survives
//! validation (`Manifest`). Validation happens at discovery, so `connector list`
//! can show a broken connector with its reason and a job naming it is `invalid`
//! instead of failing at run time.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::parse_duration;

pub const MANIFEST_FILE: &str = "pastor-connector.toml";

/// A connector or hook that says nothing gets this long.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    id: String,
    name: Option<String>,
    version: String,
    min_pastor_version: Option<String>,
    description: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    homepage: Option<String>,
    repository: Option<String>,
    license: Option<String>,
    connector: Option<ConnectorFile>,
    #[serde(default)]
    secrets: BTreeMap<String, SecretDecl>,
    #[serde(default)]
    events: Vec<HookFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectorFile {
    #[serde(default)]
    mode: Mode,
    command: Vec<String>,
    timeout: Option<String>,
    #[serde(default)]
    config: BTreeMap<String, ConfigDecl>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookFile {
    on: Vec<String>,
    #[serde(default)]
    only_own: bool,
    command: Vec<String>,
    timeout: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Run once per job run, exit when done.
    #[default]
    Poll,
    /// Started once, kept alive, items accepted at any time.
    Stream,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Poll => "poll",
            Mode::Stream => "stream",
        })
    }
}

/// The whole schema language: `required` and `description`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigDecl {
    pub required: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretDecl {
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: Version,
    pub min_pastor_version: Option<Version>,
    pub description: Option<String>,
    /// Who wrote it, as the author puts it (`Name <email>`).
    pub authors: Vec<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    /// An SPDX expression, by convention; not checked.
    pub license: Option<String>,
    pub connector: Option<ConnectorSpec>,
    pub secrets: BTreeMap<String, SecretDecl>,
    pub events: Vec<Hook>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorSpec {
    pub mode: Mode,
    pub command: Vec<String>,
    /// Bound on one poll run. A stream is not bounded by it.
    pub timeout: Duration,
    pub config: BTreeMap<String, ConfigDecl>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub on: Vec<String>,
    pub only_own: bool,
    pub command: Vec<String>,
    pub timeout: Duration,
}

impl Manifest {
    /// Parse and validate against the running pastor's version.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        Manifest::parse_for(text, &pastor_version())
    }

    pub fn parse_for(text: &str, pastor: &Version) -> Result<Manifest, String> {
        // `min_pastor_version` first, from a loose read: a manifest written
        // for a newer pastor may carry keys this one doesn't know, and the
        // strict read below would name one of those instead of saying a
        // newer pastor is needed.
        if let Ok(loose) = text.parse::<toml::Table>()
            && let Some(min) = loose.get("min_pastor_version").and_then(|v| v.as_str())
            && let Ok(min) = Version::parse(min)
            && &min > pastor
        {
            return Err(format!("needs pastor {min} or later; this is {pastor}"));
        }
        let file: ManifestFile = toml::from_str(text).map_err(|e| e.to_string())?;
        check_id(&file.id)?;
        // The catalog resolves a built-in id to the built-in, so a connector by
        // that name could never run; say so at install, not at the first job.
        if crate::connector::builtin(&file.id).is_some() {
            return Err(format!(
                "id {:?} is reserved for the built-in connector",
                file.id
            ));
        }
        let version = Version::parse(&file.version).map_err(|e| format!("version: {e}"))?;
        let min_pastor_version = file
            .min_pastor_version
            .as_deref()
            .map(|v| Version::parse(v).map_err(|e| format!("min_pastor_version: {e}")))
            .transpose()?;
        if let Some(min) = &min_pastor_version
            && min > pastor
        {
            return Err(format!("needs pastor {min} or later; this is {pastor}"));
        }
        let connector = file
            .connector
            .map(|c| -> Result<ConnectorSpec, String> {
                check_command("connector.command", &c.command)?;
                Ok(ConnectorSpec {
                    mode: c.mode,
                    command: c.command,
                    timeout: timeout_or_default("connector.timeout", c.timeout.as_deref())?,
                    config: c.config,
                })
            })
            .transpose()?;
        for name in file.secrets.keys() {
            check_env_name(name).map_err(|e| format!("secrets.{name}: {e}"))?;
        }
        let mut events = Vec::new();
        for (i, h) in file.events.into_iter().enumerate() {
            let at = format!("events[{i}]");
            if h.on.is_empty() {
                return Err(format!("{at}.on: needs at least one event type"));
            }
            for kind in &h.on {
                check_event_type(kind).map_err(|e| format!("{at}.on: {e}"))?;
            }
            check_command(&format!("{at}.command"), &h.command)?;
            events.push(Hook {
                timeout: timeout_or_default(&format!("{at}.timeout"), h.timeout.as_deref())?,
                on: h.on,
                only_own: h.only_own,
                command: h.command,
            });
        }
        if connector.is_none() && events.is_empty() {
            return Err("a connector must provide a [connector], [[events]] hooks, or both".into());
        }
        Ok(Manifest {
            name: file.name.unwrap_or_else(|| file.id.clone()),
            id: file.id,
            version,
            min_pastor_version,
            description: file.description,
            authors: file.authors,
            homepage: file.homepage,
            repository: file.repository,
            license: file.license,
            connector,
            secrets: file.secrets,
            events,
        })
    }

    /// Err(reason) when a job's `[connector]` table (minus `use`) lacks a key
    /// the manifest marks `required`. Keys it does not declare are passed
    /// through untouched: the manifest has no schema beyond `required`.
    pub fn check_config(&self, config: &Value) -> Result<(), String> {
        let Some(c) = &self.connector else {
            return Err(format!(
                "connector {:?} has no connector command (it only provides event hooks)",
                self.id
            ));
        };
        let missing: Vec<&str> = c
            .config
            .iter()
            .filter(|(k, d)| d.required && config.get(k.as_str()).is_none_or(Value::is_null))
            .map(|(k, _)| k.as_str())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(format!(
            "connector {:?} requires connector.{}",
            self.id,
            missing.join(", connector.")
        ))
    }
}

/// Connector ids are directory names under the data dir and appear in env vars
/// and `connector` commands, so they get the job names' safe alphabet.
pub fn check_id(id: &str) -> Result<(), String> {
    let first_ok = id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c));
    if first_ok && rest_ok && id.len() <= 64 {
        Ok(())
    } else {
        Err(format!("id {id:?} must match [a-z0-9][a-z0-9_.-]{{0,63}}"))
    }
}

fn check_command(field: &str, argv: &[String]) -> Result<(), String> {
    match argv.first() {
        Some(p) if !p.is_empty() => Ok(()),
        _ => Err(format!("{field}: needs a program, as an argv array")),
    }
}

fn check_env_name(name: &str) -> Result<(), String> {
    let ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err("must be an environment variable name, [A-Z_][A-Z0-9_]*".into())
    }
}

/// `task.done`, `machine.lost`: two dot-separated lowercase words. The set of
/// types belongs to the events log, so it is not closed here.
fn check_event_type(kind: &str) -> Result<(), String> {
    let word = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    match kind.split_once('.') {
        Some((a, b)) if word(a) && word(b) => Ok(()),
        _ => Err(format!("{kind:?} is not an event type like \"task.done\"")),
    }
}

fn timeout_or_default(field: &str, v: Option<&str>) -> Result<Duration, String> {
    let Some(v) = v else {
        return Ok(DEFAULT_TIMEOUT);
    };
    let d = parse_duration(v).map_err(|e| format!("{field}: {e}"))?;
    if d.is_zero() {
        return Err(format!("{field}: must not be zero"));
    }
    Ok(d)
}

/// `major.minor.patch`, numbers only: enough to compare `min_pastor_version`
/// without a semver dependency. Pre-release suffixes are rejected rather than
/// half-understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    pub fn parse(s: &str) -> Result<Version, String> {
        let parts: Vec<&str> = s.trim().split('.').collect();
        let [a, b, c] = parts.as_slice() else {
            return Err(format!("{s:?} is not major.minor.patch"));
        };
        let n = |p: &str| {
            p.parse::<u64>()
                .map_err(|_| format!("{s:?} is not major.minor.patch"))
        };
        Ok(Version(n(a)?, n(b)?, n(c)?))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// This build's version, for `min_pastor_version`. A prerelease
/// (`0.5.0-rc.1`) counts as the release it leads to, so connectors that need
/// 0.5.0 can be tried on its release candidates.
pub fn pastor_version() -> Version {
    release_part(env!("CARGO_PKG_VERSION"))
}

fn release_part(v: &str) -> Version {
    let release = v.split_once('-').map_or(v, |(r, _)| r);
    Version::parse(release).expect("the crate version starts with major.minor.patch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prerelease_counts_as_the_release_it_leads_to() {
        assert_eq!(release_part("0.5.0-rc.1"), Version(0, 5, 0));
        assert_eq!(release_part("0.5.0"), Version(0, 5, 0));
        assert!(
            Version::parse("0.5.0-rc.1").is_err(),
            "manifests still take plain versions"
        );
    }
    use serde_json::json;

    const SPEC_EXAMPLE: &str = r#"
id = "slack"
name = "Slack"
version = "0.1.0"
min_pastor_version = "0.1.0"
description = "Watch a channel, report back in thread, DM on blocked"

[connector]
mode = "poll"
command = ["bash", "poll.sh"]
timeout = "60s"

[connector.config.channel]
required = true
description = "Channel ID to watch"

[secrets.SLACK_BOT_TOKEN]
description = "Bot token with channels:history and chat:write"

[[events]]
on = ["task.done", "task.blocked", "task.failed"]
only_own = true
command = ["bash", "report.sh"]

[[events]]
on = ["task.blocked", "machine.lost"]
command = ["bash", "dm-me.sh"]
"#;

    #[test]
    fn parses_the_spec_example() {
        let m = Manifest::parse(SPEC_EXAMPLE).unwrap();
        assert_eq!(m.id, "slack");
        assert_eq!(m.name, "Slack");
        assert_eq!(m.version, Version(0, 1, 0));
        let c = m.connector.as_ref().unwrap();
        assert_eq!(c.mode, Mode::Poll);
        assert_eq!(c.command, vec!["bash", "poll.sh"]);
        assert_eq!(c.timeout, Duration::from_secs(60));
        assert!(c.config["channel"].required);
        assert!(m.secrets.contains_key("SLACK_BOT_TOKEN"));
        assert_eq!(m.events.len(), 2);
        assert!(m.events[0].only_own);
        assert!(!m.events[1].only_own);
        assert_eq!(m.events[1].on, vec!["task.blocked", "machine.lost"]);
        assert_eq!(m.events[1].timeout, DEFAULT_TIMEOUT);
    }

    /// Who wrote a connector and where it lives are optional; a manifest
    /// without them loads as before.
    #[test]
    fn authors_homepage_repository_and_license_are_optional() {
        let head = "id = \"a\"\nversion = \"0.1.0\"\n";
        let conn = "[connector]\ncommand = [\"x\"]\n";
        let m = Manifest::parse(&format!(
            "{head}authors = [\"Ana <ana@example.org>\", \"Bo\"]\nhomepage = \"https://example.org\"\nrepository = \"https://example.org/r\"\nlicense = \"MIT\"\n{conn}"
        ))
        .unwrap();
        assert_eq!(m.authors, ["Ana <ana@example.org>", "Bo"]);
        assert_eq!(m.homepage.as_deref(), Some("https://example.org"));
        assert_eq!(m.repository.as_deref(), Some("https://example.org/r"));
        assert_eq!(m.license.as_deref(), Some("MIT"));
        let m = Manifest::parse(&format!("{head}{conn}")).unwrap();
        assert!(m.authors.is_empty() && m.homepage.is_none() && m.license.is_none());
    }

    #[test]
    fn minimal_connector_and_minimal_hook() {
        let m = Manifest::parse(
            "id = \"c\"\nversion = \"1.0.0\"\n[connector]\ncommand = [\"./run\"]\n",
        )
        .unwrap();
        assert_eq!(m.name, "c", "name defaults to the id");
        let c = m.connector.unwrap();
        assert_eq!(c.mode, Mode::Poll, "mode defaults to poll");
        assert_eq!(c.timeout, DEFAULT_TIMEOUT);
        let m = Manifest::parse(
            "id = \"h\"\nversion = \"1.0.0\"\n[[events]]\non = [\"task.done\"]\ncommand = [\"x\"]\ntimeout = \"5s\"\n",
        )
        .unwrap();
        assert!(m.connector.is_none());
        assert_eq!(m.events[0].timeout, Duration::from_secs(5));
    }

    #[test]
    fn rejects_bad_manifests() {
        let base = "id = \"ok\"\nversion = \"0.1.0\"\n";
        let conn = "[connector]\ncommand = [\"x\"]\n";
        for (text, needle) in [
            (base.to_string(), "must provide"),
            (
                format!("id = \"Bad\"\nversion = \"0.1.0\"\n{conn}"),
                "must match",
            ),
            (
                format!("id = \"ok\"\nversion = \"1\"\n{conn}"),
                "major.minor.patch",
            ),
            (format!("id = \"ok\"\n{conn}"), "version"),
            (
                format!("id = \"clock\"\nversion = \"0.1.0\"\n{conn}"),
                "reserved for the built-in connector",
            ),
            (format!("{base}colour = 1\n{conn}"), "colour"),
            (
                format!("{base}[connector]\ncommand = []\n"),
                "connector.command",
            ),
            (
                format!("{base}[connector]\ncommand = [\"x\"]\nmode = \"push\"\n"),
                "push",
            ),
            (
                format!("{base}[connector]\ncommand = [\"x\"]\ntimeout = \"0s\"\n"),
                "must not be zero",
            ),
            (
                format!("{base}[connector]\ncommand = [\"x\"]\ntimeout = \"soon\"\n"),
                "connector.timeout",
            ),
            (
                format!(
                    "{base}[connector]\ncommand = [\"x\"]\n[connector.config.k]\nrequired = true\ntype = \"s\"\n"
                ),
                "type",
            ),
            (format!("{base}{conn}[secrets.token]\n"), "secrets.token"),
            (
                format!("{base}[[events]]\non = []\ncommand = [\"x\"]\n"),
                "events[0].on",
            ),
            (
                format!("{base}[[events]]\non = [\"done\"]\ncommand = [\"x\"]\n"),
                "event type",
            ),
            (
                format!("{base}[[events]]\non = [\"task.done\"]\ncommand = [\"\"]\n"),
                "events[0].command",
            ),
            (
                format!(
                    "id = \"ok\"\nversion = \"0.1.0\"\nmin_pastor_version = \"99.0.0\"\n{conn}"
                ),
                "needs pastor 99.0.0",
            ),
            // A key this pastor doesn't know yet, in a manifest for a newer
            // pastor: the version is what gets reported, not the key.
            (
                format!(
                    "id = \"ok\"\nversion = \"0.1.0\"\nmin_pastor_version = \"99.0.0\"\nfuture_key = 1\n{conn}"
                ),
                "needs pastor 99.0.0",
            ),
        ] {
            let err = Manifest::parse(&text).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}\n{text}");
        }
    }

    #[test]
    fn min_pastor_version_compares_numerically() {
        let text = "id = \"a\"\nversion = \"0.1.0\"\nmin_pastor_version = \"0.10.0\"\n[connector]\ncommand = [\"x\"]\n";
        assert!(Manifest::parse_for(text, &Version(0, 9, 0)).is_err());
        assert!(Manifest::parse_for(text, &Version(0, 10, 0)).is_ok());
        assert!(Manifest::parse_for(text, &Version(1, 0, 0)).is_ok());
    }

    #[test]
    fn check_config_enforces_required_keys_only() {
        let m = Manifest::parse(SPEC_EXAMPLE).unwrap();
        assert!(m.check_config(&json!({"channel": "C1"})).is_ok());
        assert!(
            m.check_config(&json!({"channel": "C1", "extra": 1}))
                .is_ok()
        );
        let err = m.check_config(&json!({})).unwrap_err();
        assert!(err.contains("requires connector.channel"), "{err}");
        let hooks_only = Manifest::parse(
            "id = \"n\"\nversion = \"0.1.0\"\n[[events]]\non = [\"task.done\"]\ncommand = [\"x\"]\n",
        )
        .unwrap();
        assert!(
            hooks_only
                .check_config(&json!({}))
                .unwrap_err()
                .contains("no connector")
        );
    }
}
