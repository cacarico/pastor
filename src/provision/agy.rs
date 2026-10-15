//! The agy provisioner: before a profiled agy launch, make agy's global
//! settings on the machine deny what the machine's own permission profile
//! denies (`docs/superpowers/specs/2026-10-15-agy-provisioner-design.md`).
//!
//! agy takes no permission rule from a flag or the environment, only from
//! its settings files, so the rules a profile needs must be on the machine.
//! The write is additive (rules appended to `permissions.deny`, none removed
//! or moved, no other key touched), idempotent (a file that covers every
//! rule is not written), atomic (a temp file renamed over the old one) and
//! a compare and swap against what was read. agy can still write back a
//! copy from before the edit, so this runs before every launch, not once.

use crate::config::agy::{AGY_PERMISSIONS_MISSING, missing, required_rules};
use crate::herdr::transport::{ConnectError, Connector};

/// agy's global settings, under the machine's `HOME`: the file pastor writes.
pub const SETTINGS_FILE: &str = "~/.gemini/antigravity-cli/settings.json";

/// The line the read command prints before its answer, so whatever the
/// machine's rc files print first is skipped.
const MARK: &str = "pastor-agy-settings-3b1e";

/// What the read command found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgySettings {
    /// The file's content, `None` when there is no file.
    pub content: Option<String>,
    /// Whether the path is a symlink, which pastor does not replace.
    pub link: bool,
}

/// What the write command did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgyWrite {
    /// The new content is in place.
    Written,
    /// The file no longer held what was read (agy saved meanwhile); nothing
    /// was written.
    Changed,
    /// The path is a symlink; nothing was written.
    Link,
    /// The directory, the temp file or the rename failed.
    Failed,
}

/// The shell command that prints, after `MARK`, a line `<file|none>
/// <link|plain>` and then the file's content.
pub fn read_command() -> String {
    format!(
        r#"f="$HOME/.gemini/antigravity-cli/settings.json"; printf '%s\n' {MARK}; if [ -e "$f" ]; then e=file; else e=none; fi; if [ -L "$f" ]; then l=link; else l=plain; fi; printf '%s %s\n' "$e" "$l"; if [ "$e" = file ]; then cat "$f"; fi"#
    )
}

/// Reads the read command's stdout.
pub fn parse_read(stdout: &str) -> Result<AgySettings, String> {
    let line = format!("{MARK}\n");
    let at = stdout
        .match_indices(&line)
        .map(|(i, _)| i)
        .find(|&i| i == 0 || stdout.as_bytes()[i - 1] == b'\n')
        .ok_or("no answer from the settings read")?;
    let rest = &stdout[at + line.len()..];
    let (status, content) = rest.split_once('\n').unwrap_or((rest, ""));
    let link = match status.split_whitespace().nth(1) {
        Some("link") => true,
        Some("plain") => false,
        _ => {
            return Err(format!(
                "unexpected answer from the settings read: {status:?}"
            ));
        }
    };
    match status.split_whitespace().next() {
        Some("file") => Ok(AgySettings {
            content: Some(content.to_string()),
            link,
        }),
        Some("none") => Ok(AgySettings {
            content: None,
            link,
        }),
        _ => Err(format!(
            "unexpected answer from the settings read: {status:?}"
        )),
    }
}

/// The shell command that puts `new` in place of the file when it still
/// holds `expected` (`None`: when there is still no file), answering
/// `written`, `changed`, `link` or `failed` on its last line. The temp file
/// is made under `umask 077` beside the file, so the rename is atomic.
pub fn write_command(expected: Option<&str>, new: &str) -> String {
    use crate::herdr::transport::shell_quote;
    let unchanged = match expected {
        None => r#"[ ! -e "$f" ]"#.to_string(),
        Some(text) => format!(r#"printf %s {} | cmp -s - "$f""#, shell_quote(text)),
    };
    format!(
        r#"umask 077; d="$HOME/.gemini/antigravity-cli"; f="$d/settings.json"; t="$d/.settings.json.pastor.$$"; if [ -L "$f" ]; then echo link; elif ! {unchanged}; then echo changed; elif mkdir -p "$d" && printf %s {new} > "$t" && mv -f "$t" "$f"; then echo written; else rm -f "$t"; echo failed; fi"#,
        new = shell_quote(new),
    )
}

/// Reads the write command's last line.
pub fn parse_write(stdout: &str) -> Option<AgyWrite> {
    match stdout.trim_end().lines().last().unwrap_or("").trim() {
        "written" => Some(AgyWrite::Written),
        "changed" => Some(AgyWrite::Changed),
        "link" => Some(AgyWrite::Link),
        "failed" => Some(AgyWrite::Failed),
        _ => None,
    }
}

/// A JSON value that keeps its objects' keys in file order, which
/// `serde_json::Value` sorts. Duplicate keys stay as they are.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl<'de> serde::Deserialize<'de> for Json {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        d.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> serde::de::Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Json, D::Error> {
        serde::Deserialize::deserialize(d)
    }
    fn visit_bool<E>(self, b: bool) -> Result<Json, E> {
        Ok(Json::Bool(b))
    }
    fn visit_i64<E>(self, n: i64) -> Result<Json, E> {
        Ok(Json::Number(n.into()))
    }
    fn visit_u64<E>(self, n: u64) -> Result<Json, E> {
        Ok(Json::Number(n.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<Json, E> {
        serde_json::Number::from_f64(n)
            .map(Json::Number)
            .ok_or_else(|| E::custom("a number JSON cannot hold"))
    }
    fn visit_str<E>(self, s: &str) -> Result<Json, E> {
        Ok(Json::String(s.to_string()))
    }
    fn visit_string<E>(self, s: String) -> Result<Json, E> {
        Ok(Json::String(s))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element()? {
            out.push(v);
        }
        Ok(Json::Array(out))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut out = Vec::new();
        while let Some((k, v)) = map.next_entry()? {
            out.push((k, v));
        }
        Ok(Json::Object(out))
    }
}

impl serde::Serialize for Json {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(b) => s.serialize_bool(*b),
            Json::Number(n) => n.serialize(s),
            Json::String(v) => s.serialize_str(v),
            Json::Array(xs) => s.collect_seq(xs),
            Json::Object(entries) => {
                let mut map = s.serialize_map(Some(entries.len()))?;
                for (k, v) in entries {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

/// The value under `key` in `entries`, added at the end as `make()` when
/// there is none. With the key twice, the last one, which is the one a JSON
/// reader keeps.
fn entry<'a>(
    entries: &'a mut Vec<(String, Json)>,
    key: &str,
    make: impl FnOnce() -> Json,
) -> &'a mut Json {
    let at = match entries.iter().rposition(|(k, _)| k == key) {
        Some(at) => at,
        None => {
            entries.push((key.to_string(), make()));
            entries.len() - 1
        }
    };
    &mut entries[at].1
}

/// What `merge` found to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// The file already denies every rule: nothing to write.
    Unchanged,
    /// `content` denies every rule; `added` are the rules it appended.
    Write { content: String, added: Vec<String> },
}

/// The settings file `content` (`None`: no file) with every rule of `rules`
/// that its `permissions.deny` does not cover (`config::agy::missing`)
/// appended there, in `rules`' order. Nothing else changes: no rule is
/// removed or moved, and every other key keeps its value and place. An
/// error says why the file cannot take the rules.
pub fn merge(content: Option<&str>, rules: &[String]) -> Result<Merge, String> {
    let mut root = match content {
        Some(text) if !text.trim().is_empty() => {
            serde_json::from_str::<Json>(text).map_err(|e| format!("it is not JSON ({e})"))?
        }
        _ => Json::Object(vec![]),
    };
    let Json::Object(top) = &mut root else {
        return Err("its top level is not an object".into());
    };
    let Json::Object(permissions) = entry(top, "permissions", || Json::Object(vec![])) else {
        return Err("its permissions is not an object".into());
    };
    let Json::Array(deny) = entry(permissions, "deny", || Json::Array(vec![])) else {
        return Err("its permissions.deny is not an array".into());
    };
    let have: Vec<String> = deny
        .iter()
        .filter_map(|j| match j {
            Json::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let added = missing(rules, &have);
    if added.is_empty() {
        return Ok(Merge::Unchanged);
    }
    deny.extend(added.iter().cloned().map(Json::String));
    let mut content = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    content.push('\n');
    Ok(Merge::Write { content, added })
}

/// Why `reconcile` did not leave the rules in place.
#[derive(Debug)]
pub enum ReconcileError {
    /// The machine could not be reached: a transport failure.
    Transport(ConnectError),
    /// The task is refused (`agy_permissions_missing`), with the message.
    Refused(String),
}

impl From<ConnectError> for ReconcileError {
    fn from(e: ConnectError) -> Self {
        ReconcileError::Transport(e)
    }
}

/// How many times a write is tried when agy saved the file between pastor's
/// read and write.
const ATTEMPTS: usize = 2;

/// Make agy's global settings on the machine behind `conn` deny every agy
/// rule `own_deny` (the machine's own profile's deny list, in Claude Code's
/// patterns) requires. A list that requires none reads nothing. `machine`
/// names it in messages. Refused when the file cannot be read (a `command`
/// machine), parsed or written: the message lists the rules to add by hand.
pub async fn reconcile(
    conn: &dyn Connector,
    own_deny: &[String],
    machine: &str,
) -> Result<(), ReconcileError> {
    let rules = required_rules(own_deny);
    if rules.is_empty() {
        return Ok(());
    }
    let refuse = |why: String, rules: &[String]| {
        ReconcileError::Refused(format!(
            "{AGY_PERMISSIONS_MISSING}: {why}; machine {machine}'s own profile needs these deny rules in {SETTINGS_FILE} there:\n{}",
            rules
                .iter()
                .map(|r| format!("  {r}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    };
    for _ in 0..ATTEMPTS {
        let Some(read) = conn.agy_settings().await? else {
            return Err(refuse(
                "pastor cannot read agy's settings there (a command machine, or the read failed)"
                    .into(),
                &rules,
            ));
        };
        let (content, added) = match merge(read.content.as_deref(), &rules) {
            Ok(Merge::Unchanged) => return Ok(()),
            Ok(Merge::Write { content, added }) => (content, added),
            Err(why) => {
                return Err(refuse(
                    format!("pastor cannot add to {SETTINGS_FILE}: {why}"),
                    &rules,
                ));
            }
        };
        let link = || {
            refuse(
                format!("{SETTINGS_FILE} is a symlink, which pastor does not replace"),
                &added,
            )
        };
        if read.link {
            return Err(link());
        }
        match conn
            .write_agy_settings(read.content.as_deref(), &content)
            .await?
        {
            Some(AgyWrite::Written) => {
                tracing::info!(%machine, rules = ?added, file = SETTINGS_FILE, "added agy deny rules");
                return Ok(());
            }
            Some(AgyWrite::Changed) => continue,
            Some(AgyWrite::Link) => return Err(link()),
            Some(AgyWrite::Failed) | None => {
                return Err(refuse(
                    format!("pastor could not write {SETTINGS_FILE}"),
                    &added,
                ));
            }
        }
    }
    Err(refuse(
        format!("agy kept saving {SETTINGS_FILE} while pastor wrote it"),
        &rules,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn written(m: Merge) -> (String, Vec<String>) {
        match m {
            Merge::Write { content, added } => (content, added),
            Merge::Unchanged => panic!("nothing written"),
        }
    }

    #[test]
    fn a_missing_file_gets_the_rules() {
        let (content, added) =
            written(merge(None, &strings(&["command(sudo)", "write_file(*)"])).unwrap());
        assert_eq!(added, strings(&["command(sudo)", "write_file(*)"]));
        assert_eq!(
            content,
            "{\n  \"permissions\": {\n    \"deny\": [\n      \"command(sudo)\",\n      \"write_file(*)\"\n    ]\n  }\n}\n"
        );
        assert_eq!(
            written(merge(Some(" \n"), &strings(&["command(sudo)"])).unwrap()).1,
            strings(&["command(sudo)"])
        );
    }

    /// The owner's rules stay first and in order, and every other key keeps
    /// its value and its place.
    #[test]
    fn the_owners_settings_stay_as_they_are() {
        let owner = r#"{
  "zeta": {"b": 1, "a": [true, null, 1.5]},
  "permissions": {
    "mode": "request-review",
    "allow": ["command(git log)"],
    "deny": ["command(shutdown)", 7, "command(rm -rf)"],
    "ask": ["command(git push)"]
  },
  "alpha": "x"
}"#;
        let (content, added) = written(
            merge(
                Some(owner),
                &strings(&["command(rm -rf)", "command(sudo)", "write_file(*)"]),
            )
            .unwrap(),
        );
        assert_eq!(added, strings(&["command(sudo)", "write_file(*)"]));
        let tree: Json = serde_json::from_str(&content).unwrap();
        let Json::Object(top) = &tree else { panic!() };
        let keys: Vec<&str> = top.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["zeta", "permissions", "alpha"]);
        let Json::Object(perms) = &top[1].1 else {
            panic!()
        };
        let keys: Vec<&str> = perms.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["mode", "allow", "deny", "ask"]);
        let before: serde_json::Value = serde_json::from_str(owner).unwrap();
        let after: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(before["zeta"], after["zeta"]);
        assert_eq!(before["alpha"], after["alpha"]);
        for key in ["mode", "allow", "ask"] {
            assert_eq!(
                before["permissions"][key], after["permissions"][key],
                "{key}"
            );
        }
        assert_eq!(
            after["permissions"]["deny"],
            serde_json::json!([
                "command(shutdown)",
                7,
                "command(rm -rf)",
                "command(sudo)",
                "write_file(*)"
            ])
        );
    }

    /// A deny that covers a rule (here `command(*)` and a `read_file` one
    /// for `write_file`) means nothing to write; an `ask` or `allow` entry
    /// covers nothing.
    #[test]
    fn a_covered_file_is_left_alone() {
        let file =
            r#"{"permissions":{"deny":["command(*)","read_file(/)"],"ask":["write_file(*)"]}}"#;
        assert_eq!(
            merge(
                Some(file),
                &strings(&["command(git push --force)", "write_file(/etc)"])
            )
            .unwrap(),
            Merge::Unchanged
        );
        let file = r#"{"permissions":{"allow":["command(sudo)"],"ask":["command(sudo)"]}}"#;
        assert_eq!(
            written(merge(Some(file), &strings(&["command(sudo)"])).unwrap()).1,
            strings(&["command(sudo)"])
        );
    }

    #[test]
    fn a_file_of_the_wrong_shape_is_refused() {
        let rules = strings(&["command(sudo)"]);
        for (file, why) in [
            ("[1]", "top level is not an object"),
            (r#"{"permissions": []}"#, "permissions is not an object"),
            (
                r#"{"permissions": {"deny": "x"}}"#,
                "permissions.deny is not an array",
            ),
            ("{\"permissions\": {", "not JSON"),
            ("// a comment\n{}", "not JSON"),
        ] {
            let err = merge(Some(file), &rules).unwrap_err();
            assert!(err.contains(why), "{file}: {err}");
        }
    }

    /// The key a JSON reader keeps when it is there twice is the last one.
    #[test]
    fn a_repeated_key_takes_the_last() {
        let file = r#"{"permissions":{"deny":["x"]},"permissions":{"deny":["command(sudo)"]}}"#;
        assert_eq!(
            merge(Some(file), &strings(&["command(sudo)"])).unwrap(),
            Merge::Unchanged
        );
    }

    #[test]
    fn the_read_answer_skips_what_the_rc_files_print() {
        let out = format!("welcome\n{MARK}\nfile plain\n{{\"a\":1}}\n");
        assert_eq!(
            parse_read(&out).unwrap(),
            AgySettings {
                content: Some("{\"a\":1}\n".into()),
                link: false
            }
        );
        let out = format!("{MARK}\nnone link\n");
        assert_eq!(
            parse_read(&out).unwrap(),
            AgySettings {
                content: None,
                link: true
            }
        );
        assert!(parse_read("motd only\n").is_err());
        assert!(parse_read(&format!("{MARK}\nwhat\n")).is_err());
        assert_eq!(parse_write("noise\nwritten\n"), Some(AgyWrite::Written));
        assert_eq!(parse_write("changed"), Some(AgyWrite::Changed));
        assert_eq!(parse_write(""), None);
    }

    fn sh(home: &std::path::Path, command: &str) -> String {
        let out = std::process::Command::new("sh")
            .args(["-c", command])
            .env("HOME", home)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn read(home: &std::path::Path) -> AgySettings {
        parse_read(&sh(home, &read_command())).unwrap()
    }

    fn write(home: &std::path::Path, expected: Option<&str>, new: &str) -> Option<AgyWrite> {
        parse_write(&sh(home, &write_command(expected, new)))
    }

    /// The commands under `sh` with a temp `HOME`: a write makes the
    /// directory and a 0600 file, puts the content in byte for byte, and
    /// refuses a file that changed since the read.
    #[test]
    fn the_shell_commands_read_and_swap_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            read(home.path()),
            AgySettings {
                content: None,
                link: false
            }
        );
        let first = "{\n  \"a\": \"it's $HOME `x` %s\\n\"\n}\n";
        assert_eq!(write(home.path(), None, first), Some(AgyWrite::Written));
        let file = home.path().join(".gemini/antigravity-cli/settings.json");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), first);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(read(home.path()).content.as_deref(), Some(first));
        // A write that expects no file finds one.
        assert_eq!(write(home.path(), None, "{}"), Some(AgyWrite::Changed));
        // agy saved meanwhile.
        assert_eq!(
            write(home.path(), Some("{\"old\":1}"), "{}"),
            Some(AgyWrite::Changed)
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), first);
        assert_eq!(
            write(home.path(), Some(first), "{}\n"),
            Some(AgyWrite::Written)
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}\n");
        let dir = file.parent().unwrap();
        let left: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
        assert_eq!(left.len(), 1, "no temp file left behind");
    }

    /// A symlink is read through but never replaced.
    #[test]
    fn the_shell_commands_leave_a_symlink_alone() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".gemini/antigravity-cli");
        std::fs::create_dir_all(&dir).unwrap();
        let real = home.path().join("dotfiles.json");
        std::fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("settings.json")).unwrap();
        assert_eq!(
            read(home.path()),
            AgySettings {
                content: Some("{}".into()),
                link: true
            }
        );
        assert_eq!(
            write(home.path(), Some("{}"), "{\"x\":1}"),
            Some(AgyWrite::Link)
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "{}");
    }

    proptest::proptest! {
        /// Merging the output again finds nothing to add, and the owner's
        /// rules are still there, first and in order.
        #[test]
        fn prop_merge_is_idempotent_and_keeps_the_owners_rules(
            owner in proptest::collection::vec("(command|write_file|read_file)\\([a-z /*]{1,8}\\)", 0..5),
            rules in proptest::collection::vec("(command|write_file|read_url)\\([a-z /*]{1,8}\\)", 0..5),
            other in "[a-z]{1,6}",
        ) {
            let file = serde_json::json!({
                other.clone(): {"k": [1, 2]},
                "permissions": {"allow": ["command(ls)"], "deny": owner.clone()},
            })
            .to_string();
            match merge(Some(&file), &rules).unwrap() {
                Merge::Unchanged => {
                    proptest::prop_assert!(missing(&rules, &owner).is_empty());
                }
                Merge::Write { content, added } => {
                    proptest::prop_assert!(!added.is_empty());
                    proptest::prop_assert_eq!(merge(Some(&content), &rules).unwrap(), Merge::Unchanged);
                    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
                    let deny: Vec<String> = v["permissions"]["deny"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|s| s.as_str().unwrap().to_string())
                        .collect();
                    proptest::prop_assert_eq!(&deny[..owner.len()], &owner[..]);
                    proptest::prop_assert_eq!(&v["permissions"]["allow"], &serde_json::json!(["command(ls)"]));
                    proptest::prop_assert_eq!(&v[other.as_str()], &serde_json::json!({"k": [1, 2]}));
                }
            }
        }
    }
}
