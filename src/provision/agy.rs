//! The agy provisioner: before a profiled agy launch, make agy's global
//! settings on the machine say what the machine's own permission profile
//! says.
//!
//! agy takes no permission rule from a flag or the environment, only from
//! its settings files, so the rules a profile needs must be on the machine.
//! Pastor owns the permission object: `permissions.allow`, `.deny` and
//! `.ask`, `toolPermission`, `agentMode`, and, when the profile sets them,
//! `allowNonWorkspaceAccess` and `queuedMessages`. It writes them whole from
//! the profile, so a rule the profile no longer has goes, and so does one agy
//! saved from an "always allow" answer. Every other key (`trustedWorkspaces`,
//! `model`, colours) keeps its value and its place. The write is idempotent
//! (a file that already says it is not written), atomic (a temp file renamed
//! over the old one) and a compare and swap against what was read. agy can
//! still write back a copy from before the edit, so this runs before every
//! launch, not once.

use crate::config::agy::{
    AGY_PERMISSIONS_MISSING, Policy, TASK_DONE_RULE, allow_rules, required_rules,
};
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
    /// Another process holds the file's lock (`write_command_of` with
    /// `lock`); nothing was written.
    Busy,
    /// The directory, the temp file or the rename failed.
    Failed,
}

/// `path` as a shell word: `~/x` is under the machine's `$HOME`, anything
/// else is quoted as written.
pub fn path_word(path: &str) -> String {
    use crate::herdr::transport::shell_quote;
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None if path == "~" => "\"$HOME\"".to_string(),
        None => shell_quote(path),
    }
}

/// The shell command that prints, after `MARK`, a line `<file|none>
/// <link|plain>` and then the file's content.
pub fn read_command() -> String {
    read_command_of(SETTINGS_FILE)
}

/// `read_command` for any file: `~/` is the machine's home. What the trust
/// writer reads Claude's and agy's stores with (`trust_write`).
pub fn read_command_of(path: &str) -> String {
    let word = path_word(path);
    format!(
        r#"f={word}; printf '%s\n' {MARK}; if [ -e "$f" ]; then e=file; else e=none; fi; if [ -L "$f" ]; then l=link; else l=plain; fi; printf '%s %s\n' "$e" "$l"; if [ "$e" = file ]; then cat "$f"; fi"#
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

/// A shell command and what to feed its standard input. The content of a file
/// never goes on a command line, where any local user can read it from
/// `/proc` and the kernel caps one word at 128 KiB: it goes on stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteCommand {
    pub command: String,
    pub input: String,
}

/// The shell command that puts `new` in place of the file when it still
/// holds `expected` (`None`: when there is still no file), answering
/// `written`, `changed`, `link`, `busy` or `failed` on its last line. See
/// `write_command_of`.
pub fn write_command(expected: Option<&str>, new: &str) -> WriteCommand {
    write_command_of(SETTINGS_FILE, expected, new, false)
}

/// `write_command` for any file: `~/` is the machine's home. `new` is the
/// command's standard input; `expected` is a sha256 in the command (a sum
/// tells nothing of the file). The byte count and sha256 of `new` are in the
/// command too (they are not secret): a stdin cut short, as when the ssh
/// client dies mid-transfer, leaves a temp file that matches neither, and the
/// answer is `failed` with the file untouched. The temp file is made under `umask 077`
/// beside the file, holds the whole new content and has the file's mode
/// before anything is compared, so what is left between the compare and the
/// rename is one `mv`, which is atomic. With `lock`, the compare and the
/// rename run holding `<file>.lock`, a directory made with `mkdir`, which is
/// the lock a program that saves the file under a lock of that name takes
/// (`busy` after two seconds of waiting for it).
pub fn write_command_of(path: &str, expected: Option<&str>, new: &str, lock: bool) -> WriteCommand {
    let word = path_word(path);
    let unchanged = match expected {
        None => r#"[ ! -e "$f" ]"#.to_string(),
        Some(text) => format!(r#"[ "$(h 2>/dev/null < "$f")" = {} ]"#, sha256_hex(text)),
    };
    let (take, give) = if lock {
        (
            r#"got=; for n in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do if mkdir "$f.lock" 2>/dev/null; then got=1; break; fi; sleep 0.1; done"#,
            r#"rmdir "$f.lock";"#,
        )
    } else {
        ("got=1", "")
    };
    let (len, sum) = (new.len(), sha256_hex(new));
    let command = format!(
        r#"umask 077; f={word}; d=$(dirname "$f"); t="$d/.pastor-write.$$"; h() {{ if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -d' ' -f1; }}; r=failed; if [ -L "$f" ]; then cat >/dev/null; r=link; elif mkdir -p "$d" && cat > "$t" && [ "$(wc -c < "$t" | tr -d ' ')" = {len} ] && [ "$(h < "$t")" = {sum} ] && {{ [ ! -e "$f" ] || chmod "$(stat -c %a "$f" 2>/dev/null || stat -f %Lp "$f")" "$t"; }}; then {take}; if [ -z "$got" ]; then r=busy; else if {unchanged}; then if mv -f "$t" "$f"; then r=written; fi; else r=changed; fi; {give} fi; fi; [ "$r" = written ] || rm -f "$t"; echo "$r""#
    );
    WriteCommand {
        command,
        input: new.to_string(),
    }
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Reads the write command's last line.
pub fn parse_write(stdout: &str) -> Option<AgyWrite> {
    match stdout.trim_end().lines().last().unwrap_or("").trim() {
        "written" => Some(AgyWrite::Written),
        "changed" => Some(AgyWrite::Changed),
        "link" => Some(AgyWrite::Link),
        "busy" => Some(AgyWrite::Busy),
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
    /// The file already says it all: nothing to write.
    Unchanged,
    /// `content` says it; `dropped` are the rules the file had in
    /// `permissions` that it no longer lists.
    Write {
        content: String,
        dropped: Vec<String>,
    },
}

/// Set `key` in `entries` to `value`, in the place the key has (the last, if
/// it is there twice), else at the end.
fn set(entries: &mut Vec<(String, Json)>, key: &str, value: Json) {
    *entry(entries, key, || Json::Null) = value;
}

fn strings(rules: &[String]) -> Json {
    Json::Array(rules.iter().cloned().map(Json::String).collect())
}

/// The settings file `content` (`None`: no file) as it is once it says
/// `policy`: `permissions.allow` and `.deny` the policy's rules (Claude Code's
/// patterns as agy's rules, `allow_rules` and `required_rules`), `.ask`
/// empty if the file has one, `toolPermission` and `agentMode` the options'
/// or their defaults, the other two options when set. Nothing else changes:
/// every other key keeps its value and place. An error says why the file
/// cannot take the policy.
pub fn merge(content: Option<&str>, policy: &Policy) -> Result<Merge, String> {
    let mut root = match content {
        Some(text) if !text.trim().is_empty() => {
            serde_json::from_str::<Json>(text).map_err(|e| format!("it is not JSON ({e})"))?
        }
        _ => Json::Object(vec![]),
    };
    let before = root.clone();
    let Json::Object(top) = &mut root else {
        return Err("its top level is not an object".into());
    };
    let options = &policy.options;
    let mut dropped: Vec<String> = Vec::new();
    {
        let Json::Object(permissions) = entry(top, "permissions", || Json::Object(vec![])) else {
            return Err("its permissions is not an object".into());
        };
        let mut allow = allow_rules(&policy.allow);
        // Whatever the profile, a task may end itself.
        if !allow.iter().any(|r| r == TASK_DONE_RULE) {
            allow.push(TASK_DONE_RULE.to_string());
        }
        let deny = required_rules(&policy.deny);
        for (key, rules) in [
            ("allow", Some(&allow)),
            ("deny", Some(&deny)),
            ("ask", None),
        ] {
            if rules.is_none() && !permissions.iter().any(|(k, _)| k == key) {
                continue;
            }
            let none = Vec::new();
            let rules = rules.unwrap_or(&none);
            let old = entry(permissions, key, || Json::Null);
            if let Json::Array(old) = &*old {
                dropped.extend(old.iter().filter_map(|j| match j {
                    Json::String(s) if !rules.contains(s) => Some(s.clone()),
                    _ => None,
                }));
            }
            *old = strings(rules);
        }
    }
    set(
        top,
        "toolPermission",
        Json::String(
            options
                .tool_permission
                .clone()
                .unwrap_or_else(|| "always-proceed".into()),
        ),
    );
    set(
        top,
        "agentMode",
        Json::String(
            options
                .agent_mode
                .clone()
                .unwrap_or_else(|| "accept-edits".into()),
        ),
    );
    if let Some(on) = options.allow_non_workspace_access {
        set(top, "allowNonWorkspaceAccess", Json::Bool(on));
    }
    if let Some(mode) = &options.queued_messages {
        set(top, "queuedMessages", Json::String(mode.clone()));
    }
    if root == before {
        return Ok(Merge::Unchanged);
    }
    let mut content = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    content.push('\n');
    Ok(Merge::Write { content, dropped })
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

/// Make agy's global settings on the machine behind `conn` say what `own`,
/// the machine's own permission profile, says (`merge`). `None`, a machine
/// with no profile, reads nothing and writes nothing. `machine` names it in
/// messages. When the file cannot be read (a `command` machine), parsed or
/// written, the task is refused with the deny rules to add by hand, unless the
/// profile denies nothing: then no rule is missing that could matter, since a
/// profiled launch approves every `ask`, and the failure is only logged.
pub async fn reconcile(
    conn: &dyn Connector,
    own: Option<&Policy>,
    machine: &str,
) -> Result<(), ReconcileError> {
    let Some(policy) = own else {
        return Ok(());
    };
    let rules = required_rules(&policy.deny);
    let give_up = |why: String| {
        if rules.is_empty() {
            tracing::warn!(%machine, file = SETTINGS_FILE, %why, "agy's permissions not written");
            return Ok(());
        }
        Err(ReconcileError::Refused(format!(
            "{AGY_PERMISSIONS_MISSING}: {why}; machine {machine}'s own profile needs these deny rules in {SETTINGS_FILE} there:\n{}",
            rules
                .iter()
                .map(|r| format!("  {r}"))
                .collect::<Vec<_>>()
                .join("\n")
        )))
    };
    for _ in 0..ATTEMPTS {
        let Some(read) = conn.agy_settings().await? else {
            return give_up(
                "pastor cannot read agy's settings there (a command machine, or the read failed)"
                    .into(),
            );
        };
        let (content, dropped) = match merge(read.content.as_deref(), policy) {
            Ok(Merge::Unchanged) => return Ok(()),
            Ok(Merge::Write { content, dropped }) => (content, dropped),
            Err(why) => return give_up(format!("pastor cannot write {SETTINGS_FILE}: {why}")),
        };
        let link = || {
            give_up(format!(
                "{SETTINGS_FILE} is a symlink, which pastor does not replace"
            ))
        };
        if read.link {
            return link();
        }
        match conn
            .write_agy_settings(read.content.as_deref(), &content)
            .await?
        {
            Some(AgyWrite::Written) => {
                tracing::info!(%machine, ?dropped, file = SETTINGS_FILE, "wrote agy's permissions");
                return Ok(());
            }
            Some(AgyWrite::Changed | AgyWrite::Busy) => continue,
            Some(AgyWrite::Link) => return link(),
            Some(AgyWrite::Failed) | None => {
                return give_up(format!("pastor could not write {SETTINGS_FILE}"));
            }
        }
    }
    give_up(format!(
        "agy kept saving {SETTINGS_FILE} while pastor wrote it"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agy::Options;

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn policy(allow: &[&str], deny: &[&str]) -> Policy {
        Policy {
            allow: strings(allow),
            deny: strings(deny),
            options: Options::default(),
        }
    }

    fn written(m: Merge) -> (String, Vec<String>) {
        match m {
            Merge::Write { content, dropped } => (content, dropped),
            Merge::Unchanged => panic!("nothing written"),
        }
    }

    fn value(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    /// No file, or an empty one, gets the whole object: both lists, and the
    /// two words a profiled launch runs under.
    #[test]
    fn a_missing_file_gets_the_whole_object() {
        let p = policy(
            &["Edit", "Bash(git log:*)"],
            &["Bash(sudo:*)", "Edit(/etc/**)"],
        );
        let (content, dropped) = written(merge(None, &p).unwrap());
        assert!(dropped.is_empty());
        assert_eq!(
            content,
            "{\n  \"permissions\": {\n    \"allow\": [\n      \"write_file(*)\",\n      \"command(git log)\",\n      \"command(pastor task done)\"\n    ],\n    \"deny\": [\n      \"command(sudo)\",\n      \"write_file(/etc)\"\n    ]\n  },\n  \"toolPermission\": \"always-proceed\",\n  \"agentMode\": \"accept-edits\"\n}\n"
        );
        assert_eq!(written(merge(Some(" \n"), &p).unwrap()).0, content);
    }

    /// The profile's lists replace the file's whole: a stale allow, a deny the
    /// profile dropped, and the rules agy saved from "always allow" answers
    /// go, and are named in `dropped`. An `ask` is emptied, and every other
    /// key keeps its value and its place.
    #[test]
    fn a_stale_allow_is_removed_and_the_other_keys_stay() {
        let file = r#"{
  "zeta": {"b": 1, "a": [true, null, 1.5]},
  "trustedWorkspaces": ["/w/one", "/w/two"],
  "permissions": {
    "mode": "request-review",
    "allow": ["command(git log)", "command(old tool)", 7],
    "deny": ["command(shutdown)", "command(sudo)"],
    "ask": ["command(git push)"]
  },
  "alpha": "x"
}"#;
        let p = policy(&["Bash(git log:*)", "Read"], &["Bash(sudo:*)"]);
        let (content, dropped) = written(merge(Some(file), &p).unwrap());
        assert_eq!(
            dropped,
            strings(&[
                "command(old tool)",
                "command(shutdown)",
                "command(git push)"
            ])
        );
        let after = value(&content);
        let before = value(file);
        assert_eq!(
            after["permissions"]["allow"],
            serde_json::json!([
                "command(git log)",
                "read_file(*)",
                "command(pastor task done)"
            ])
        );
        assert_eq!(
            after["permissions"]["deny"],
            serde_json::json!(["command(sudo)"])
        );
        assert_eq!(after["permissions"]["ask"], serde_json::json!([]));
        for key in ["zeta", "alpha", "trustedWorkspaces"] {
            assert_eq!(before[key], after[key], "{key}");
        }
        assert_eq!(before["permissions"]["mode"], after["permissions"]["mode"]);
        let tree: Json = serde_json::from_str(&content).unwrap();
        let Json::Object(top) = &tree else { panic!() };
        let keys: Vec<&str> = top.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "zeta",
                "trustedWorkspaces",
                "permissions",
                "alpha",
                "toolPermission",
                "agentMode"
            ]
        );
        let Json::Object(perms) = &top[2].1 else {
            panic!()
        };
        let keys: Vec<&str> = perms.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["mode", "allow", "deny", "ask"]);
    }

    /// A profile change reaches the file on the next apply: the second
    /// profile's lists replace the first's, and the same profile again
    /// writes nothing.
    #[test]
    fn a_profile_change_reaches_the_file_and_the_same_profile_writes_nothing() {
        let first = policy(&["Edit", "Bash"], &["Bash(sudo:*)"]);
        let (one, _) = written(merge(None, &first).unwrap());
        assert_eq!(merge(Some(&one), &first).unwrap(), Merge::Unchanged);
        let second = policy(&["Read"], &["Edit", "Bash(sudo:*)"]);
        let (two, dropped) = written(merge(Some(&one), &second).unwrap());
        assert_eq!(dropped, strings(&["write_file(*)", "command(*)"]));
        let v = value(&two);
        assert_eq!(
            v["permissions"]["allow"],
            serde_json::json!(["read_file(*)", "command(pastor task done)"])
        );
        assert_eq!(
            v["permissions"]["deny"],
            serde_json::json!(["write_file(*)", "command(sudo)"])
        );
        assert_eq!(merge(Some(&two), &second).unwrap(), Merge::Unchanged);
    }

    /// An allow agy cannot state as the pattern says is left out rather than
    /// widened: `Bash(git log)` is exact in Claude and a prefix in agy. A
    /// tool agy has no rule for adds nothing.
    #[test]
    fn an_allow_agy_cannot_state_exactly_is_left_out() {
        let p = policy(
            &["Bash(git log)", "Bash(git log:*)", "Glob", "WebSearch"],
            &[],
        );
        let (content, _) = written(merge(None, &p).unwrap());
        assert_eq!(
            value(&content)["permissions"]["allow"],
            serde_json::json!(["command(git log)", "command(pastor task done)"])
        );
    }

    /// Whatever the profile, an agy task may end itself: the rule for
    /// `pastor task done` is in the allow list, once, even when the profile
    /// allows no Bash or already names it.
    #[test]
    fn every_agy_profile_may_end_itself() {
        let done = serde_json::json!([TASK_DONE_RULE]);
        let allow =
            |p: &Policy| value(&written(merge(None, p).unwrap()).0)["permissions"]["allow"].clone();
        assert_eq!(allow(&policy(&[], &[])), done);
        assert_eq!(
            allow(&policy(&["Read"], &["Edit"])),
            serde_json::json!(["read_file(*)", TASK_DONE_RULE])
        );
        assert_eq!(allow(&policy(&["Bash(pastor task done:*)"], &[])), done);
    }

    /// The two words default to what a profiled launch runs under; the other
    /// two are written only when the profile sets them, and the profile's
    /// word replaces the file's.
    #[test]
    fn the_profiles_words_set_the_scalar_keys() {
        let file = r#"{"toolPermission":"request-review","agentMode":"auto","allowNonWorkspaceAccess":true,"queuedMessages":"queue"}"#;
        let mut p = policy(&[], &[]);
        let after = value(&written(merge(Some(file), &p).unwrap()).0);
        assert_eq!(after["toolPermission"], "always-proceed");
        assert_eq!(after["agentMode"], "accept-edits");
        assert_eq!(after["allowNonWorkspaceAccess"], true, "left as it was");
        assert_eq!(after["queuedMessages"], "queue", "left as it was");
        p.options = Options {
            tool_permission: Some("request-review".into()),
            agent_mode: Some("auto".into()),
            allow_non_workspace_access: Some(false),
            queued_messages: Some("send-immediately".into()),
        };
        let after = value(&written(merge(Some(file), &p).unwrap()).0);
        assert_eq!(after["toolPermission"], "request-review");
        assert_eq!(after["agentMode"], "auto");
        assert_eq!(after["allowNonWorkspaceAccess"], false);
        assert_eq!(after["queuedMessages"], "send-immediately");
        let none = value(&written(merge(None, &p).unwrap()).0);
        assert_eq!(none["queuedMessages"], "send-immediately");
        let none = value(&written(merge(None, &policy(&[], &[])).unwrap()).0);
        assert!(none.get("queuedMessages").is_none());
        assert!(none.get("allowNonWorkspaceAccess").is_none());
    }

    #[test]
    fn a_file_of_the_wrong_shape_is_refused() {
        let p = policy(&[], &["Bash(sudo:*)"]);
        for (file, why) in [
            ("[1]", "top level is not an object"),
            (r#"{"permissions": []}"#, "permissions is not an object"),
            ("{\"permissions\": {", "not JSON"),
            ("// a comment\n{}", "not JSON"),
        ] {
            let err = merge(Some(file), &p).unwrap_err();
            assert!(err.contains(why), "{file}: {err}");
        }
        // A list of a shape agy would not read is ours to replace.
        let file = r#"{"permissions": {"deny": "x", "allow": {"a": 1}}}"#;
        let (content, _) = written(merge(Some(file), &p).unwrap());
        assert_eq!(
            value(&content)["permissions"]["deny"],
            serde_json::json!(["command(sudo)"])
        );
    }

    /// The key a JSON reader keeps when it is there twice is the last one.
    #[test]
    fn a_repeated_key_takes_the_last() {
        let p = policy(&[], &["Bash(sudo:*)"]);
        let file = r#"{"permissions":{"deny":["x"]},"permissions":{"deny":["command(sudo)"],"allow":["command(pastor task done)"]},"toolPermission":"always-proceed","agentMode":"accept-edits"}"#;
        assert_eq!(merge(Some(file), &p).unwrap(), Merge::Unchanged);
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
        sh_feeding(home, command, "")
    }

    /// `command` under `sh -c` with `input` on its stdin.
    fn sh_feeding(home: &std::path::Path, command: &str, input: &str) -> String {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .args(["-c", command])
            .env("HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_string();
        let feeder = std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
        let out = child.wait_with_output().unwrap();
        feeder.join().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn run_write(home: &std::path::Path, w: &WriteCommand) -> Option<AgyWrite> {
        parse_write(&sh_feeding(home, &w.command, &w.input))
    }

    fn read(home: &std::path::Path) -> AgySettings {
        parse_read(&sh(home, &read_command())).unwrap()
    }

    fn write(home: &std::path::Path, expected: Option<&str>, new: &str) -> Option<AgyWrite> {
        run_write(home, &write_command(expected, new))
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

    /// The same commands for any file under `~/` or an absolute path: the
    /// file's directory is made, an existing file keeps its mode, a path with
    /// a quote or a space is one word, and the compare still refuses a file
    /// that changed.
    #[test]
    fn the_shell_commands_take_any_path_and_keep_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let read = |p: &str| parse_read(&sh(home.path(), &read_command_of(p))).unwrap();
        let write = |p: &str, expected: Option<&str>, new: &str| {
            run_write(home.path(), &write_command_of(p, expected, new, false))
        };
        let path = "~/it's here/.claude.json";
        assert_eq!(read(path).content, None);
        assert_eq!(write(path, None, "{\"a\":1}\n"), Some(AgyWrite::Written));
        let file = home.path().join("it's here/.claude.json");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"a\":1}\n");
        assert_eq!(read(path).content.as_deref(), Some("{\"a\":1}\n"));
        // An existing file keeps the mode it had.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            write(path, Some("{\"a\":1}\n"), "{\"a\":2}\n"),
            Some(AgyWrite::Written)
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o644
        );
        // It changed meanwhile.
        assert_eq!(write(path, Some("old"), "x"), Some(AgyWrite::Changed));
        // An absolute path is taken as written.
        let abs = home.path().join("abs.json");
        std::fs::write(&abs, "{}").unwrap();
        let abs = abs.to_str().unwrap();
        assert_eq!(read(abs).content.as_deref(), Some("{}"));
        assert_eq!(write(abs, Some("{}"), "{\"b\":1}"), Some(AgyWrite::Written));
        assert_eq!(std::fs::read_to_string(abs).unwrap(), "{\"b\":1}");
        let left: Vec<_> = std::fs::read_dir(home.path().join("it's here"))
            .unwrap()
            .collect();
        assert_eq!(left.len(), 1, "no temp file left behind");
    }

    /// The content of a file goes on stdin, never into the command: a store
    /// can hold keys that any local user reads from `/proc/<pid>/cmdline`,
    /// and the kernel refuses a word of 128 KiB or more. The command is the
    /// same size whatever the file holds, and a 200 KB store is swapped.
    #[test]
    fn the_content_goes_on_stdin_not_in_the_command() {
        let home = tempfile::tempdir().unwrap();
        let secret = "sk-secret-token";
        let big = |tag: &str, n: usize| {
            format!(
                "{{\"key\": \"{secret}{tag}\", \"pad\": \"{}\"}}\n",
                "x".repeat(n)
            )
        };
        let (old, new) = (big("old", 200_000), big("new", 200_000));
        let w = write_command_of("~/.claude.json", Some(&old), &new, true);
        assert!(!w.command.contains(secret), "{}", w.command);
        assert!(w.command.len() < 2_000, "{} bytes", w.command.len());
        assert_eq!(w.input, new);
        let path = home.path().join(".claude.json");
        std::fs::write(&path, &old).unwrap();
        assert_eq!(run_write(home.path(), &w), Some(AgyWrite::Written));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
        // The compare is on the whole file: one changed byte refuses.
        let w = write_command_of("~/.claude.json", Some(&old), &old, true);
        assert_eq!(run_write(home.path(), &w), Some(AgyWrite::Changed));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), new);
        // And what is read comes back whole.
        let read = parse_read(&sh(home.path(), &read_command_of("~/.claude.json"))).unwrap();
        assert_eq!(read.content.as_deref(), Some(new.as_str()));
    }

    /// A stdin that stops short (the ssh client died mid-transfer) never
    /// replaces the file: the temp file does not match the byte count and sum
    /// the command carries, and is removed. Cut at any length, including to
    /// nothing and to one byte short, with and without the lock.
    #[test]
    fn a_cut_short_stdin_leaves_the_file_untouched() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".claude.json");
        let old = format!("{{\"old\": \"{}\"}}\n", "o".repeat(10_000));
        let new = format!("{{\"new\": \"{}\"}}\n", "n".repeat(10_000));
        std::fs::write(&file, &old).unwrap();
        for lock in [true, false] {
            let w = write_command_of("~/.claude.json", Some(&old), &new, lock);
            for cut in [0, 1, new.len() / 2, new.len() - 1] {
                let short = WriteCommand {
                    input: new[..cut].to_string(),
                    ..w.clone()
                };
                assert_eq!(
                    run_write(home.path(), &short),
                    Some(AgyWrite::Failed),
                    "{cut}"
                );
                assert_eq!(std::fs::read_to_string(&file).unwrap(), old, "{cut}");
                let left: Vec<_> = std::fs::read_dir(home.path()).unwrap().collect();
                assert_eq!(left.len(), 1, "no temp file or lock left behind: {left:?}");
            }
            // Whole, it lands; restore for the next round.
            assert_eq!(run_write(home.path(), &w), Some(AgyWrite::Written));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), new);
            std::fs::write(&file, &old).unwrap();
        }
        // Same length, other bytes: the sum refuses it.
        let w = write_command_of("~/.claude.json", Some(&old), &new, false);
        let other = WriteCommand {
            input: new.replace('n', "m"),
            ..w
        };
        assert_eq!(run_write(home.path(), &other), Some(AgyWrite::Failed));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), old);
    }

    /// With `lock`, the write holds `<file>.lock` (a directory, as a program
    /// that saves the file under a lock of that name makes it) from before
    /// the compare to after the rename, and gives it back; while another
    /// holds it, nothing is written and the answer is `busy`.
    #[test]
    fn the_write_takes_the_lock_a_saving_program_takes() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".claude.json");
        std::fs::write(&file, "{}").unwrap();
        let lock = home.path().join(".claude.json.lock");
        let w = write_command_of("~/.claude.json", Some("{}"), "{\"a\":1}", true);
        assert_eq!(run_write(home.path(), &w), Some(AgyWrite::Written));
        assert!(!lock.exists(), "the lock is given back");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"a\":1}");

        std::fs::create_dir(&lock).unwrap();
        let w = write_command_of("~/.claude.json", Some("{\"a\":1}"), "{}", true);
        assert_eq!(run_write(home.path(), &w), Some(AgyWrite::Busy));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"a\":1}");
        assert!(lock.exists(), "another's lock is left as it was");
        let left: Vec<_> = std::fs::read_dir(home.path()).unwrap().collect();
        assert_eq!(left.len(), 2, "no temp file left behind");

        // A lock released while it waits is taken after.
        let release = lock.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            std::fs::remove_dir(release).unwrap();
        });
        assert_eq!(run_write(home.path(), &w), Some(AgyWrite::Written));
        t.join().unwrap();

        // Without `lock`, a held lock is none of its business.
        std::fs::create_dir(&lock).unwrap();
        let w = write_command_of("~/.claude.json", Some("{}"), "{\"b\":1}", false);
        assert_eq!(run_write(home.path(), &w), Some(AgyWrite::Written));
        assert!(lock.exists());
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
        /// Merging the output again finds nothing to write, and every key
        /// the policy does not own is as it was.
        #[test]
        fn prop_merge_is_idempotent_and_keeps_the_other_keys(
            owner in proptest::collection::vec("(command|write_file|read_file)\\([a-z /*]{1,8}\\)", 0..5),
            allow in proptest::collection::vec("(Edit|Read|Bash|Bash\\(git [a-z]{1,4}:\\*\\))", 0..4),
            deny in proptest::collection::vec("(Edit|Write|Bash\\(sudo:\\*\\)|WebFetch)", 0..4),
            other in "[a-z]{1,6}",
        ) {
            let file = serde_json::json!({
                other.clone(): {"k": [1, 2]},
                "permissions": {"allow": ["command(ls)"], "deny": owner},
            })
            .to_string();
            let p = Policy { allow, deny, options: Options::default() };
            let Merge::Write { content, .. } = merge(Some(&file), &p).unwrap() else {
                proptest::prop_assert!(false, "a file with no toolPermission is changed");
                unreachable!()
            };
            proptest::prop_assert_eq!(merge(Some(&content), &p).unwrap(), Merge::Unchanged);
            let v: serde_json::Value = serde_json::from_str(&content).unwrap();
            proptest::prop_assert_eq!(&v[other.as_str()], &serde_json::json!({"k": [1, 2]}));
        }
    }
}
