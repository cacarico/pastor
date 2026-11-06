//! Harness version pins: the version of an agent's CLI a flock expects on its
//! machines (`pins` in a `[[flock]]` entry, which a `[[machine]]` entry may
//! override), and the drift between a pin and what a machine runs.
//!
//! Nothing here installs or changes anything: `machine list` shows the drift
//! the machine's actor last saw, `machine check` has a `harness-version` row
//! for each agent, and `machine install` (`harness_install`) brings a machine
//! to its pins. The binary and
//! the command that asks for the version come from the harness manifests
//! (`harness_manifest`); a pin can name a kind whose manifest has a version.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The word that pins nothing: any version will do.
pub const LATEST: &str = "latest";

/// The id of the row `machine check` has for each agent of a pinned kind.
pub const VERSION_ROW: &str = "harness-version";

/// A pin per kind, as `pins` writes it.
pub type Pins = BTreeMap<String, String>;

/// The version in a CLI's `--version` output, on its last line: the first
/// word that starts with a digit and is made of digits, dots and the usual pre-release
/// letters (`2.1.310 (Claude Code)` is `2.1.310`, `v1.4.7` is `1.4.7`). `None`
/// for output with no such word, and for `none`.
pub fn parse_version(output: &str) -> Option<String> {
    // The answer is the last line: rc files print before it.
    let line = output.lines().rev().find(|l| !l.trim().is_empty())?;
    line.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches('v');
        let starts = word.chars().next().is_some_and(|c| c.is_ascii_digit());
        let fits = word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
        (starts && fits).then(|| word.to_string())
    })
}

/// Does `actual` satisfy `pin`? Every dot-separated part of the pin must
/// equal the part of `actual` in the same place, so `2.1` takes `2.1.310`
/// and `2.1.310` takes only itself. `latest` takes any version.
pub fn satisfies(pin: &str, actual: &str) -> bool {
    if pin == LATEST {
        return true;
    }
    let mut have = actual.split('.');
    pin.split('.').all(|part| have.next() == Some(part))
}

/// The load-time check of a `pins` table: a kind whose built-in manifest says
/// how to read its version (a harness only in the config dir cannot be pinned),
/// and a version that is a word with no spaces. `key` names the table in
/// the error.
pub fn check_pins(key: &str, pins: &Pins) -> Result<(), String> {
    for (kind, version) in pins {
        let builtin = crate::harness_manifest::Harnesses::builtin();
        if !builtin.versioned().any(|m| &m.id == kind) {
            let kinds: Vec<&str> = builtin.versioned().map(|m| m.id.as_str()).collect();
            return Err(format!(
                "{key}.{kind}: pins take {} for now, not {kind:?}",
                kinds.join(" and ")
            ));
        }
        let good = version == LATEST
            || version
                .split('.')
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
        if !good {
            return Err(format!(
                "{key}.{kind}: {version:?} is not a version (digits and dots, such as 2.1.310 or 2.1) or {LATEST:?}"
            ));
        }
    }
    Ok(())
}

/// One pinned kind on one machine, as its actor last saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessStatus {
    pub kind: String,
    /// What the flock (or the machine) pins.
    pub pin: String,
    /// What the machine runs; `None` when the CLI is not there or the
    /// machine has not answered yet.
    pub version: Option<String>,
    /// The machine answered, and what it runs is not the pin. A kind not
    /// installed is not drift (the install row says that); `latest` never
    /// drifts.
    pub drift: bool,
}

impl HarnessStatus {
    pub fn new(kind: &str, pin: &str, version: Option<String>) -> HarnessStatus {
        let drift = version.as_deref().is_some_and(|v| !satisfies(pin, v));
        HarnessStatus {
            kind: kind.to_string(),
            pin: pin.to_string(),
            version,
            drift,
        }
    }

    /// `claude 2.1.0, pinned 2.1.310`.
    pub fn say(&self) -> String {
        format!(
            "{} {}, pinned {}",
            self.kind,
            self.version.as_deref().unwrap_or("-"),
            self.pin
        )
    }
}

/// The HARNESS cell of `machine list`: `-` with nothing pinned, `ok`, or
/// `drift` and the kinds that have.
pub fn cell(status: &[HarnessStatus]) -> String {
    if status.is_empty() {
        return "-".into();
    }
    let drifted: Vec<&str> = status
        .iter()
        .filter(|s| s.drift)
        .map(|s| s.kind.as_str())
        .collect();
    if drifted.is_empty() {
        "ok".into()
    } else {
        format!("drift {}", drifted.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_out_of_what_a_cli_prints() {
        for (out, want) in [
            ("2.1.310 (Claude Code)\n", Some("2.1.310")),
            ("1.4.7\n", Some("1.4.7")),
            ("v1.4.7", Some("1.4.7")),
            ("welcome\nagy version 1.4.7-rc.1\n", Some("1.4.7-rc.1")),
            ("5 files\nnone", None),
            ("none", None),
            ("", None),
            ("claude: command not found", None),
        ] {
            assert_eq!(parse_version(out).as_deref(), want, "{out:?}");
        }
    }

    #[test]
    fn a_pin_takes_its_own_parts_and_latest_takes_any() {
        for (pin, actual, yes) in [
            ("2.1.310", "2.1.310", true),
            ("2.1.310", "2.1.309", false),
            ("2.1.310", "2.1", false),
            ("2.1", "2.1.310", true),
            ("2.1", "2.10.0", false),
            ("2", "2.9.9", true),
            ("latest", "0.0.1", true),
        ] {
            assert_eq!(satisfies(pin, actual), yes, "{pin} / {actual}");
        }
    }

    #[test]
    fn pins_name_claude_or_agy_and_a_version() {
        let pins = |kv: &[(&str, &str)]| -> Pins {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        check_pins("pins", &pins(&[("claude", "2.1.310"), ("agy", "latest")])).unwrap();
        for (kv, says) in [
            (&[("codex", "1.0")][..], "agy and claude for now"),
            (&[("claude", "")][..], "is not a version"),
            (&[("claude", "two")][..], "is not a version"),
            (&[("claude", "2 1")][..], "is not a version"),
            (&[("claude", "1.x")][..], "is not a version"),
            (&[("claude", "2..1")][..], "is not a version"),
            (&[("claude", "2.")][..], "is not a version"),
            (&[("claude", "2.1.310abc")][..], "is not a version"),
        ] {
            let err = check_pins("flock a: pins", &pins(kv)).unwrap_err();
            assert!(
                err.contains(says) && err.starts_with("flock a: pins."),
                "{err}"
            );
        }
    }

    #[test]
    fn status_and_cell() {
        let ok = HarnessStatus::new("claude", "2.1", Some("2.1.310".into()));
        let bad = HarnessStatus::new("agy", "1.4.7", Some("1.4.2".into()));
        let missing = HarnessStatus::new("agy", "1.4.7", None);
        assert!(!ok.drift && bad.drift && !missing.drift);
        assert_eq!(bad.say(), "agy 1.4.2, pinned 1.4.7");
        assert_eq!(cell(&[]), "-");
        assert_eq!(cell(std::slice::from_ref(&ok)), "ok");
        assert_eq!(cell(&[ok, bad, missing]), "drift agy");
    }
}
