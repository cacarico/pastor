//! `pastor-provisioner.toml`: the file's shape (`ManifestFile`) and what
//! survives validation (`Manifest`). A provisioner is a list of checks a
//! machine must pass to run agents, each with the command that checks it
//! and, optionally, the command that would fix it. Validation happens when
//! the catalog is read, so a machine naming a broken provisioner fails the
//! flock's load with the reason, not its first `machine check`.
//!
//! Nothing here runs a command. The `fix` of a check is parsed and checked
//! like its `command`, and kept for the runner.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::connector::manifest::{Version, check_command, check_id, pastor_version};

pub const MANIFEST_FILE: &str = "pastor-provisioner.toml";

/// The herdr agent kinds `src/config` knows: what a check's `agent` may
/// name.
pub const AGENT_KINDS: [&str; 4] = [
    "claude",
    "codex",
    crate::config::opencode::KIND,
    crate::config::agy::KIND,
];

/// A check that says nothing gets this long, and its fix the same.
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
    platform: Option<PlatformFile>,
    #[serde(default)]
    check: Vec<CheckFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlatformFile {
    os: Os,
    #[serde(default)]
    distro: Vec<String>,
    detect: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckFile {
    id: String,
    description: Option<String>,
    command: Vec<String>,
    fix: Option<Vec<String>>,
    #[serde(default)]
    needs: Vec<String>,
    account: Option<Account>,
    timeout: Option<String>,
    agent: Option<String>,
}

/// What `uname -s` says, lowercased: the platforms pastor builds for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Darwin,
    Linux,
    Freebsd,
}

impl std::fmt::Display for Os {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Os::Darwin => "darwin",
            Os::Linux => "linux",
            Os::Freebsd => "freebsd",
        })
    }
}

/// An account a flock's machines must be logged into: `accounts` on a
/// `[[flock]]`, and `account` on a provisioner's check, which then runs only
/// for a machine whose flock lists it. The set is closed: pastor knows how
/// each is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Account {
    Github,
    Claude,
}

impl Account {
    pub const ALL: [Account; 2] = [Account::Github, Account::Claude];

    pub fn as_str(self) -> &'static str {
        match self {
            Account::Github => "github",
            Account::Claude => "claude",
        }
    }
}

impl std::fmt::Display for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: Version,
    pub min_pastor_version: Option<Version>,
    pub description: Option<String>,
    pub authors: Vec<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    pub platform: Platform,
    /// In file order, which is also a valid run order: a check comes after
    /// every check it `needs`.
    pub checks: Vec<Check>,
}

/// The machines a provisioner is for. `machine check` compares it with what
/// the machine reports and refuses a mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    /// `ID` values of `/etc/os-release` the provisioner accepts (`arch`,
    /// `debian`); empty accepts any. Linux only.
    pub distro: Vec<String>,
    /// Exit 0 when the provisioner fits the machine, run after `os` and
    /// `distro` match. Validated like a check's `command`.
    pub detect: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub id: String,
    pub description: Option<String>,
    /// Exit 0 passes.
    pub command: Vec<String>,
    /// What would make the check pass. Validated like `command`; the runner
    /// is later work.
    pub fix: Option<Vec<String>>,
    /// Checks that must pass first; a failed one skips this one.
    pub needs: Vec<String>,
    /// Set: the check runs only for a machine whose flock lists the account.
    pub account: Option<Account>,
    /// Bounds the command, and the fix on its own.
    pub timeout: Duration,
    /// Set: a kind from `AGENT_KINDS`, and the check runs once per agent of
    /// that kind that can land on the machine, with that agent's env
    /// (`run::AgentEnv`).
    pub agent: Option<String>,
}

impl Manifest {
    /// Parse and validate against the running pastor's version.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        Manifest::parse_for(text, &pastor_version())
    }

    pub fn parse_for(text: &str, pastor: &Version) -> Result<Manifest, String> {
        // `min_pastor_version` first, from a loose read, as for a connector:
        // a manifest for a newer pastor may carry keys this one lacks.
        if let Ok(loose) = text.parse::<toml::Table>()
            && let Some(min) = loose.get("min_pastor_version").and_then(|v| v.as_str())
            && let Ok(min) = Version::parse(min)
            && &min > pastor
        {
            return Err(format!("needs pastor {min} or later; this is {pastor}"));
        }
        let file: ManifestFile = toml::from_str(text).map_err(|e| e.to_string())?;
        check_id(&file.id)?;
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
        let Some(platform) = file.platform else {
            return Err("needs a [platform] with the os it is for".into());
        };
        if !platform.distro.is_empty() && platform.os != Os::Linux {
            return Err(format!(
                "platform.distro: only linux has distributions, not {}",
                platform.os
            ));
        }
        for d in &platform.distro {
            if d.is_empty() || d.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!(
                    "platform.distro: {d:?} is not an os-release ID (one word, no spaces)"
                ));
            }
        }
        if let Some(detect) = &platform.detect {
            check_command("platform.detect", detect)?;
        }
        if file.check.is_empty() {
            return Err("needs at least one [[check]]".into());
        }
        let mut ids = BTreeSet::new();
        for (i, c) in file.check.iter().enumerate() {
            let at = format!("check[{i}]");
            check_check_id(&c.id).map_err(|e| format!("{at}.id: {e}"))?;
            if !ids.insert(c.id.as_str()) {
                return Err(format!("{at}.id: {:?} is used twice", c.id));
            }
        }
        let mut checks = Vec::with_capacity(file.check.len());
        let mut before: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, c) in file.check.iter().enumerate() {
            let at = format!("check[{i}]");
            check_command(&format!("{at}.command"), &c.command)?;
            if let Some(fix) = &c.fix {
                check_command(&format!("{at}.fix"), fix)?;
            }
            if let Some(agent) = &c.agent
                && !AGENT_KINDS.contains(&agent.as_str())
            {
                return Err(format!("{at}.agent: {agent:?} is not a known agent kind"));
            }
            for need in &c.needs {
                if need == &c.id {
                    return Err(format!("{at}.needs: {:?} needs itself", c.id));
                }
                // Only earlier checks: file order is then a run order, and
                // a cycle cannot be written.
                match before.get(need.as_str()) {
                    Some(_) => {}
                    None if ids.contains(need.as_str()) => {
                        return Err(format!(
                            "{at}.needs: {need:?} comes later in the file; a check needs only checks before it"
                        ));
                    }
                    None => return Err(format!("{at}.needs: no check is called {need:?}")),
                }
            }
            let timeout = timeout_or_default(&format!("{at}.timeout"), c.timeout.as_deref())?;
            before.insert(c.id.as_str(), i);
            checks.push(Check {
                id: c.id.clone(),
                description: c.description.clone(),
                command: c.command.clone(),
                fix: c.fix.clone(),
                needs: c.needs.clone(),
                account: c.account,
                timeout,
                agent: c.agent.clone(),
            });
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
            platform: Platform {
                os: platform.os,
                distro: platform.distro,
                detect: platform.detect,
            },
            checks,
        })
    }

    /// The checks a machine in flocks asking for `accounts` runs: every
    /// check with no account, and those whose account is asked for.
    pub fn checks_for(&self, accounts: &[Account]) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.account.is_none_or(|a| accounts.contains(&a)))
            .collect()
    }

    /// The checks for one agent kind, in file order: each with `agent` set
    /// to it.
    pub fn checks_for_kind(&self, kind: &str) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.agent.as_deref() == Some(kind))
            .collect()
    }
}

/// Check ids name a check in `machine check` output and in `needs`; the
/// connector id alphabet, without the dot that would read as a path.
pub(crate) fn check_check_id(id: &str) -> Result<(), String> {
    let first_ok = id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-".contains(c));
    if first_ok && rest_ok && id.len() <= 64 {
        Ok(())
    } else {
        Err(format!("{id:?} must match [a-z0-9][a-z0-9_-]{{0,63}}"))
    }
}

fn timeout_or_default(field: &str, v: Option<&str>) -> Result<Duration, String> {
    let Some(v) = v else {
        return Ok(DEFAULT_TIMEOUT);
    };
    let d = crate::config::parse_duration(v).map_err(|e| format!("{field}: {e}"))?;
    if d.is_zero() {
        return Err(format!("{field}: must not be zero"));
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
id = "macos"
name = "macOS"
version = "0.1.0"
min_pastor_version = "0.1.0"
description = "A Mac that runs agents under herdr"
authors = ["Ana <ana@example.org>"]
license = "MIT"

[platform]
os = "darwin"

[[check]]
id = "brew"
description = "Homebrew is installed"
command = ["sh", "-c", "command -v brew"]

[[check]]
id = "herdr"
description = "herdr is on the PATH of a non-interactive shell"
command = ["sh", "-c", "command -v herdr"]
fix = ["brew", "install", "herdr"]
needs = ["brew"]
timeout = "5m"

[[check]]
id = "gh-login"
description = "gh is logged in"
command = ["gh", "auth", "status"]
account = "github"
"#;

    #[test]
    fn parses_the_example() {
        let m = Manifest::parse(EXAMPLE).unwrap();
        assert_eq!(m.id, "macos");
        assert_eq!(m.name, "macOS");
        assert_eq!(m.version, Version(0, 1, 0));
        assert_eq!(m.min_pastor_version, Some(Version(0, 1, 0)));
        assert_eq!(m.authors, ["Ana <ana@example.org>"]);
        assert_eq!(m.license.as_deref(), Some("MIT"));
        assert_eq!(m.platform.os, Os::Darwin);
        assert!(m.platform.distro.is_empty());
        assert_eq!(m.checks.len(), 3);
        let brew = &m.checks[0];
        assert_eq!(brew.id, "brew");
        assert_eq!(brew.command, ["sh", "-c", "command -v brew"]);
        assert!(brew.fix.is_none() && brew.needs.is_empty() && brew.account.is_none());
        assert_eq!(brew.timeout, DEFAULT_TIMEOUT);
        let herdr = &m.checks[1];
        assert_eq!(herdr.fix.as_deref().unwrap(), ["brew", "install", "herdr"]);
        assert_eq!(herdr.needs, ["brew"]);
        assert_eq!(herdr.timeout, Duration::from_secs(300));
        assert_eq!(m.checks[2].account, Some(Account::Github));
    }

    #[test]
    fn name_defaults_to_the_id_and_a_linux_platform_may_name_distros() {
        let m = Manifest::parse(
            "id = \"arch\"\nversion = \"1.0.0\"\n[platform]\nos = \"linux\"\ndistro = [\"arch\"]\n[[check]]\nid = \"a\"\ncommand = [\"true\"]\n",
        )
        .unwrap();
        assert_eq!(m.name, "arch");
        assert_eq!(m.platform.os, Os::Linux);
        assert_eq!(m.platform.distro, ["arch"]);
        assert!(m.description.is_none() && m.homepage.is_none());
    }

    /// A check with an account runs only for a flock that asks for it;
    /// the others always.
    #[test]
    fn checks_for_filters_by_account() {
        let m = Manifest::parse(EXAMPLE).unwrap();
        let ids = |accounts: &[Account]| {
            m.checks_for(accounts)
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&[]), ["brew", "herdr"]);
        assert_eq!(ids(&[Account::Claude]), ["brew", "herdr"]);
        assert_eq!(ids(&[Account::Github]), ["brew", "herdr", "gh-login"]);
    }

    #[test]
    fn rejects_bad_manifests() {
        let head = "id = \"ok\"\nversion = \"0.1.0\"\n";
        let plat = "[platform]\nos = \"linux\"\n";
        let check = "[[check]]\nid = \"a\"\ncommand = [\"true\"]\n";
        for (text, needle) in [
            (format!("version = \"0.1.0\"\n{plat}{check}"), "id"),
            (
                format!("id = \"Bad\"\nversion = \"0.1.0\"\n{plat}{check}"),
                "must match",
            ),
            (format!("id = \"ok\"\n{plat}{check}"), "version"),
            (
                format!("id = \"ok\"\nversion = \"1\"\n{plat}{check}"),
                "major.minor.patch",
            ),
            (
                format!("{head}min_pastor_version = \"99.0.0\"\n{plat}{check}"),
                "needs pastor 99.0.0",
            ),
            (
                format!("{head}min_pastor_version = \"99.0.0\"\nfuture_key = 1\n{plat}{check}"),
                "needs pastor 99.0.0",
            ),
            (format!("{head}colour = 1\n{plat}{check}"), "colour"),
            (format!("{head}{check}"), "[platform]"),
            (format!("{head}[platform]\n{check}"), "os"),
            (
                format!("{head}[platform]\nos = \"windows\"\n{check}"),
                "windows",
            ),
            (
                format!("{head}[platform]\nos = \"darwin\"\ndistro = [\"arch\"]\n{check}"),
                "only linux has distributions",
            ),
            (
                format!("{head}[platform]\nos = \"linux\"\ndistro = [\"arch linux\"]\n{check}"),
                "os-release ID",
            ),
            (
                format!("{head}[platform]\nos = \"linux\"\narch = \"arm64\"\n{check}"),
                "arch",
            ),
            (format!("{head}{plat}"), "at least one [[check]]"),
            (
                format!("{head}{plat}[[check]]\ncommand = [\"true\"]\n"),
                "id",
            ),
            (
                format!("{head}{plat}[[check]]\nid = \"a.b\"\ncommand = [\"true\"]\n"),
                "check[0].id",
            ),
            (
                format!("{head}{plat}{check}[[check]]\nid = \"a\"\ncommand = [\"true\"]\n"),
                "used twice",
            ),
            (
                format!("{head}{plat}[[check]]\nid = \"a\"\ncommand = []\n"),
                "check[0].command",
            ),
            (
                format!("{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"\"]\n"),
                "check[0].command",
            ),
            (format!("{head}{plat}[[check]]\nid = \"a\"\n"), "command"),
            (
                format!("{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\nfix = []\n"),
                "check[0].fix",
            ),
            (
                format!(
                    "{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\nneeds = [\"a\"]\n"
                ),
                "needs itself",
            ),
            (
                format!(
                    "{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\nneeds = [\"zz\"]\n"
                ),
                "no check is called \"zz\"",
            ),
            (
                format!(
                    "{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\nneeds = [\"b\"]\n[[check]]\nid = \"b\"\ncommand = [\"true\"]\n"
                ),
                "comes later in the file",
            ),
            (
                format!(
                    "{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\naccount = \"slack\"\n"
                ),
                "slack",
            ),
            (
                format!(
                    "{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\nagent = \"slack\"\n"
                ),
                "not a known agent kind",
            ),
            (
                format!("{head}[platform]\nos = \"linux\"\ndetect = []\n{check}"),
                "platform.detect",
            ),
            (
                format!(
                    "{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\ntimeout = \"0s\"\n"
                ),
                "check[0].timeout: must not be zero",
            ),
            (
                format!(
                    "{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\ntimeout = \"soon\"\n"
                ),
                "check[0].timeout",
            ),
            (
                format!("{head}{plat}[[check]]\nid = \"a\"\ncommand = [\"true\"]\nrun = true\n"),
                "run",
            ),
        ] {
            let err = Manifest::parse(&text).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}\n{text}");
        }
    }

    /// A check's `agent` names one of the kinds `src/config` knows, and
    /// `checks_for_kind` lists the checks for one.
    #[test]
    fn agent_on_a_check_names_a_known_kind() {
        let m = Manifest::parse(
            "id = \"a\"\nversion = \"0.1.0\"\n[platform]\nos = \"linux\"\n[[check]]\nid = \"a\"\ncommand = [\"true\"]\nagent = \"claude\"\n[[check]]\nid = \"b\"\ncommand = [\"true\"]\nagent = \"opencode\"\n[[check]]\nid = \"c\"\ncommand = [\"true\"]\n",
        )
        .unwrap();
        assert_eq!(m.checks[0].agent.as_deref(), Some("claude"));
        assert_eq!(m.checks[1].agent.as_deref(), Some("opencode"));
        assert!(m.checks[2].agent.is_none());
        let ids = |kind| {
            m.checks_for_kind(kind)
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("claude"), ["a"]);
        assert_eq!(ids("opencode"), ["b"]);
        assert!(ids("codex").is_empty());
    }

    /// `detect` on `[platform]` is an optional argv, checked after `os` and
    /// `distro` match.
    #[test]
    fn detect_on_platform_is_an_optional_argv() {
        let m = Manifest::parse(
            "id = \"a\"\nversion = \"0.1.0\"\n[platform]\nos = \"linux\"\ndetect = [\"sh\", \"-c\", \"test -f /etc/rpi-issue\"]\n[[check]]\nid = \"a\"\ncommand = [\"true\"]\n",
        )
        .unwrap();
        assert_eq!(
            m.platform.detect.as_deref(),
            Some(
                &[
                    "sh".to_string(),
                    "-c".to_string(),
                    "test -f /etc/rpi-issue".to_string()
                ][..]
            )
        );
        let m = Manifest::parse(
            "id = \"a\"\nversion = \"0.1.0\"\n[platform]\nos = \"linux\"\n[[check]]\nid = \"a\"\ncommand = [\"true\"]\n",
        )
        .unwrap();
        assert!(m.platform.detect.is_none());
    }

    /// A cycle through `needs` cannot be written: each check may need only
    /// checks before it, so file order is a run order.
    #[test]
    fn needs_point_backwards_only() {
        let m = Manifest::parse(
            "id = \"ok\"\nversion = \"0.1.0\"\n[platform]\nos = \"linux\"\n[[check]]\nid = \"a\"\ncommand = [\"true\"]\n[[check]]\nid = \"b\"\ncommand = [\"true\"]\nneeds = [\"a\"]\n[[check]]\nid = \"c\"\ncommand = [\"true\"]\nneeds = [\"a\", \"b\"]\n",
        )
        .unwrap();
        assert_eq!(m.checks[2].needs, ["a", "b"]);
    }

    #[test]
    fn min_pastor_version_compares_numerically() {
        let text = "id = \"a\"\nversion = \"0.1.0\"\nmin_pastor_version = \"0.10.0\"\n[platform]\nos = \"linux\"\n[[check]]\nid = \"a\"\ncommand = [\"true\"]\n";
        assert!(Manifest::parse_for(text, &Version(0, 9, 0)).is_err());
        assert!(Manifest::parse_for(text, &Version(0, 10, 0)).is_ok());
    }

    #[test]
    fn accounts_are_github_and_claude() {
        assert_eq!(Account::Github.to_string(), "github");
        assert_eq!(Account::Claude.to_string(), "claude");
        assert_eq!(
            Account::ALL.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            ["github", "claude"]
        );
        for a in Account::ALL {
            let back: Account =
                toml::from_str::<toml::Table>(&format!("a = \"{a}\"")).unwrap()["a"]
                    .clone()
                    .try_into()
                    .unwrap();
            assert_eq!(back, a);
        }
    }
}
