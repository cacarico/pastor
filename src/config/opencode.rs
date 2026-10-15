//! A permission profile for an opencode agent. opencode has no flags for
//! tool lists; it reads its permission rules from its config, and from
//! `OPENCODE_PERMISSION`, a JSON object merged over it. So a task under a
//! profile gets the profile's lists (in Claude Code's patterns, which the
//! profiles are written in) translated into that object, and its pane gets
//! it with the variables that point opencode at another config emptied
//! and the repo's own config turned off (`Agents::launch`), the repo's
//! instruction file passed back by path (`instructions_content`). Dispatch
//! reads the machine's own global opencode config and `~/.opencode`
//! (`CONFIG_READ_COMMAND`, `parse_permission_keys`) and denies every
//! permission key found there in the same object, under every profile,
//! `unrestricted` included (`permission_json`'s `never_allow`), rather than
//! refuse the task. Dispatch still refuses with `OPENCODE_PERMISSIONS_CONFLICT`
//! when the managed config or, on macOS, the managed preferences set rules
//! (`CONFIG_CHECK_COMMAND`): those are merged after `OPENCODE_CONFIG_CONTENT`,
//! so pastor cannot neutralise them, and when a file it did find could not be
//! parsed, since it then cannot say what it would let through.

use std::collections::{BTreeMap, BTreeSet};

/// The herdr agent kind this applies to.
pub const KIND: &str = "opencode";

/// The variable opencode merges over its config's `permission`.
pub const PERMISSION_ENV: &str = "OPENCODE_PERMISSION";

/// The variables that point opencode at a config of their own. A profiled
/// task's pane gets each set empty, which opencode reads as unset, so an
/// `[agents]` env or the machine's shell cannot add rules around the
/// profile's; dispatch then fills `CONFIG_CONTENT_ENV` with instructions
/// only.
pub const CONFIG_ENV: [&str; 3] = [
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_CONFIG_CONTENT",
];

/// The variable that stops opencode reading the project's config: an
/// `opencode.json` or `.opencode/` in the checkout, which a branch under
/// review could fill with rules that lift the profile's denies. It stops
/// opencode reading the repo's `AGENTS.md` and `CLAUDE.md` too, which
/// `instructions_content` gives back.
pub const DISABLE_PROJECT_CONFIG_ENV: &str = "OPENCODE_DISABLE_PROJECT_CONFIG";

/// The variable that holds the config a profiled task's pane gets: only
/// its instructions (`instructions_content`).
pub const CONFIG_CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// The repo files opencode reads as instructions, in the order it looks for
/// them: it reads the first one there and stops.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Where a profiled task working in `dir` may find its instruction files,
/// in `INSTRUCTION_FILES` order. Dispatch passes on the first one that is
/// there (`instructions_content`).
pub fn instruction_candidates(dir: &str) -> Vec<String> {
    let dir = dir.trim_end_matches('/');
    INSTRUCTION_FILES
        .iter()
        .map(|f| format!("{dir}/{f}"))
        .collect()
}

/// `OPENCODE_CONFIG_CONTENT` for a profiled task: `files` as
/// `instructions`. By absolute path, since with the project config off
/// opencode looks for a relative one in its global config dir; a file that
/// is not there matches nothing. Every listed file is read, unlike opencode's
/// own search, so dispatch lists only the first one there, or every
/// candidate when it cannot tell.
pub fn instructions_content(files: &[String]) -> String {
    serde_json::json!({ "instructions": files }).to_string()
}

/// The code of a profiled opencode task sent to a machine whose managed
/// opencode config (or managed preferences) has permission rules pastor
/// cannot neutralise, or whose global config or `~/.opencode` has rules
/// pastor found but could not parse.
pub const OPENCODE_PERMISSIONS_CONFLICT: &str = "opencode_permissions_conflict";

/// opencode permissions a profile always allows: the agent's own to-do list,
/// which touches nothing outside it.
const ALWAYS_ALLOWED: [&str; 2] = ["todoread", "todowrite"];

/// The opencode permissions a Claude Code tool name stands for. `None` for a
/// tool opencode has no permission for; everything is denied unless allowed,
/// so leaving one out never lets more through.
fn permissions(tool: &str) -> Option<&'static [&'static str]> {
    Some(match tool {
        "Read" => &["read", "list"],
        "Glob" => &["glob"],
        "Grep" => &["grep"],
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => &["edit"],
        "Bash" => &["bash"],
        "WebFetch" => &["webfetch"],
        "WebSearch" => &["websearch"],
        "Task" => &["task"],
        _ => return None,
    })
}

/// A Claude Code pattern as opencode permissions and the patterns under
/// each: `Edit` is `edit` on `*`; `Bash(git log:*)` is `bash` on `git log`
/// and `git log *`, Claude's `:*` being a prefix match on the words; any
/// other argument goes as written.
fn translate(pattern: &str) -> Vec<(&'static str, Vec<String>)> {
    let (tool, arg) = match pattern.split_once('(') {
        Some((tool, rest)) => (tool, rest.strip_suffix(')')),
        None => (pattern, None),
    };
    let Some(perms) = permissions(tool) else {
        return vec![];
    };
    let patterns = match arg {
        None => vec!["*".to_string()],
        Some(arg) if tool == "Bash" => match arg.strip_suffix(":*") {
            Some(prefix) => vec![prefix.to_string(), format!("{prefix} *")],
            None => vec![arg.to_string()],
        },
        Some(arg) => vec![arg.to_string()],
    };
    perms.iter().map(|p| (*p, patterns.clone())).collect()
}

/// `OPENCODE_PERMISSION` for a task's settled lists (`Profiles::apply`
/// already put the profile's first and dropped what is denied from allow),
/// plus `never_allow`: opencode permission keys (its own names, such as
/// `"bash"`, found on the machine itself, outside any profile) that must
/// stay denied no matter what the profile says, `unrestricted` included.
///
/// opencode takes the last rule that matches, in the object's order, and
/// asks about nothing a rule denies, so the object opens with `"*": "deny"`
/// (a tool no pattern allows is refused, not asked about), or with
/// `"*": "allow"` when `open` (profile `unrestricted`, every tool: opencode
/// has tools no Claude name maps to, such as MCP ones), then each
/// permission with its allows before its denies, then `never_allow`'s keys
/// last of all: each one gets its own `"*": "deny"`, which `add` moves to
/// the end of that permission's rules, so it is the last one opencode
/// checks and so the one that wins, whatever the profile allowed for the
/// same key (`unrestricted`'s open fallback included) — except the
/// always-allowed to-do list, which stays allowed regardless; a machine's
/// config is not expected to name it, and a profile's own choice to always
/// allow it should not be second-guessed by the machine's unrelated rules.
/// A permission with only `*` is written as its action alone. Built by
/// hand, since the order is the meaning and serde_json sorts its maps.
pub fn permission_json(
    allow: &[String],
    deny: &[String],
    open: bool,
    never_allow: &[String],
) -> String {
    let mut order: Vec<String> = vec![];
    let mut rules: BTreeMap<String, Vec<(String, &'static str)>> = BTreeMap::new();
    let mut add = |perm: &str, pattern: String, action: &'static str| {
        let list = rules.entry(perm.to_string()).or_insert_with(|| {
            order.push(perm.to_string());
            vec![]
        });
        // One key per pattern: a later rule takes the earlier one's place
        // and moves to the end, where it wins.
        list.retain(|(p, _)| *p != pattern);
        list.push((pattern, action));
    };
    for perm in ALWAYS_ALLOWED {
        add(perm, "*".into(), "allow");
    }
    for (list, action) in [(allow, "allow"), (deny, "deny")] {
        for pattern in list {
            for (perm, patterns) in translate(pattern) {
                for p in patterns {
                    add(perm, p, action);
                }
            }
        }
    }
    for perm in never_allow {
        if ALWAYS_ALLOWED.contains(&perm.as_str()) {
            continue;
        }
        add(perm, "*".into(), "deny");
    }
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let fallback = if open { "allow" } else { "deny" };
    let mut out = format!("{{{}:{}", quote("*"), quote(fallback));
    for perm in &order {
        let list = &rules[perm];
        let value = match list.as_slice() {
            [(p, action)] if p == "*" => quote(action),
            _ => format!(
                "{{{}}}",
                list.iter()
                    .map(|(p, a)| format!("{}:{}", quote(p), quote(a)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        };
        out.push_str(&format!(",{}:{value}", quote(perm)));
    }
    out.push('}');
    out
}

/// The shell command that answers `yes` when the managed opencode config
/// sets permission rules, `no` otherwise: the managed dir (`/etc/opencode`,
/// on macOS `/Library/Application Support/opencode`, or opencode's own
/// `OPENCODE_TEST_MANAGED_CONFIG_DIR`)'s `opencode.json` and
/// `opencode.jsonc`, and on macOS the MDM preferences opencode merges last,
/// `ai.opencode.managed.plist` under `/Library/Managed Preferences/<user>`
/// and `/Library/Managed Preferences` (`PASTOR_TEST_MANAGED_PREFERENCES_DIR`
/// points the tests elsewhere), read as JSON through `plutil`; one `plutil`
/// cannot read counts as a yes. Any `"permission"` key counts, top level or
/// under an agent, and a `"tools"` one, which opencode turns into rules;
/// one in a comment too: a false yes refuses a task with the reason, a
/// false no would run it under rules nobody chose. These two sources are
/// merged after `OPENCODE_CONFIG_CONTENT`, so pastor cannot neutralise them
/// the way it does the global config and `~/.opencode`
/// (`CONFIG_READ_COMMAND`); the project's config is off for a profiled task
/// (`DISABLE_PROJECT_CONFIG_ENV`) and is not read by either command.
pub const CONFIG_CHECK_COMMAND: &str = r#"o="$(uname -s)"; m="$OPENCODE_TEST_MANAGED_CONFIG_DIR"; if [ -z "$m" ]; then if [ "$o" = Darwin ]; then m="/Library/Application Support/opencode"; else m=/etc/opencode; fi; fi; k='"(permission|tools)"[[:space:]]*:'; r=no; if cat "$m/opencode.json" "$m/opencode.jsonc" 2>/dev/null | grep -Eq "$k"; then r=yes; fi; if [ "$o" = Darwin ]; then p="${PASTOR_TEST_MANAGED_PREFERENCES_DIR:-/Library/Managed Preferences}"; for f in "$p/$(id -un)/ai.opencode.managed.plist" "$p/ai.opencode.managed.plist"; do if [ -e "$f" ]; then if j="$(plutil -convert json -o - "$f" 2>/dev/null)"; then if printf '%s' "$j" | grep -Eq "$k"; then r=yes; fi; else r=yes; fi; fi; done; fi; printf '%s' "$r""#;

/// The shell command that prints the content of every one of the machine's
/// own global opencode config files and `~/.opencode`'s that exists:
/// `config.json`, `opencode.json` and `opencode.jsonc` under
/// `$XDG_CONFIG_HOME/opencode` (else `~/.config/opencode`), then the same
/// three under `~/.opencode`. Each file's content is preceded by a
/// `===<path>===` line of its own so `parse_permission_keys` can tell them
/// apart; a file that is not there prints nothing. The managed dir and
/// macOS managed preferences are not read here: `CONFIG_CHECK_COMMAND`
/// covers those, since pastor cannot neutralise what they set.
pub const CONFIG_READ_COMMAND: &str = r#"d="${XDG_CONFIG_HOME:-$HOME/.config}/opencode"; h="$HOME/.opencode"; for f in "$d/config.json" "$d/opencode.json" "$d/opencode.jsonc" "$h/config.json" "$h/opencode.json" "$h/opencode.jsonc"; do if [ -f "$f" ]; then printf '===%s===\n' "$f"; cat "$f"; printf '\n'; fi; done"#;

/// What a profiled opencode task's env must always deny on top of its
/// profile, from the machine's own global opencode config and
/// `~/.opencode` (`CONFIG_READ_COMMAND`, read through `parse_permission_keys`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MachineDenyKeys {
    /// Nothing was found there, or it cannot be known (a `command` machine).
    #[default]
    None,
    /// Every opencode permission key found, to deny under every profile
    /// (`permission_json`'s `never_allow`).
    Keys(Vec<String>),
    /// A file was found but could not be parsed as JSONC, so pastor cannot
    /// say what it would let through; the task refuses instead of running
    /// unprotected.
    Unreadable,
}

/// Reads `CONFIG_READ_COMMAND`'s output into `MachineDenyKeys`: splits it on
/// its `===<path>===` markers, and for each file's content that is not
/// blank, strips JSONC (`strip_jsonc`) and parses it as JSON, collecting
/// every permission key (`collect_permission_keys`). A file that fails to
/// parse makes the whole answer `Unreadable`, since one bad file leaves
/// pastor unable to say what the others would still let through.
pub fn parse_permission_keys(output: &str) -> MachineDenyKeys {
    let mut keys = BTreeSet::new();
    let mut current: Option<String> = None;
    for line in output.split('\n') {
        if line.starts_with("===") && line.ends_with("===") && line.len() > 6 {
            if let Some(content) = current.take()
                && collect_keys_from_file(&content, &mut keys).is_err()
            {
                return MachineDenyKeys::Unreadable;
            }
            current = Some(String::new());
            continue;
        }
        if let Some(content) = &mut current {
            content.push_str(line);
            content.push('\n');
        }
    }
    if let Some(content) = current
        && collect_keys_from_file(&content, &mut keys).is_err()
    {
        return MachineDenyKeys::Unreadable;
    }
    if keys.is_empty() {
        MachineDenyKeys::None
    } else {
        MachineDenyKeys::Keys(keys.into_iter().collect())
    }
}

/// Parses one file's content as JSONC and adds its permission keys to
/// `keys`. `Err` when the content is not blank but does not parse as JSON
/// once comments and trailing commas are stripped.
fn collect_keys_from_file(content: &str, keys: &mut BTreeSet<String>) -> Result<(), ()> {
    if content.trim().is_empty() {
        return Ok(());
    }
    let value: serde_json::Value = serde_json::from_str(&strip_jsonc(content)).map_err(|_| ())?;
    collect_permission_keys(&value, keys);
    Ok(())
}

/// Every key of `container.<field>`, when it is there and an object.
fn object_keys(container: &serde_json::Value, field: &str, keys: &mut BTreeSet<String>) {
    if let Some(map) = container.get(field).and_then(serde_json::Value::as_object) {
        keys.extend(map.keys().cloned());
    }
}

/// The opencode permission keys set anywhere in a parsed config file: its
/// top-level `permission` and legacy `tools` objects, and the `permission`
/// object under each entry of `agent` and `mode` (opencode's per-agent and
/// per-mode overrides). Only the key names are kept (`"bash"`, `"edit"`,
/// ...), not their patterns: a key found anywhere is denied everywhere
/// (`permission_json`'s `never_allow`).
fn collect_permission_keys(v: &serde_json::Value, keys: &mut BTreeSet<String>) {
    let Some(obj) = v.as_object() else {
        return;
    };
    object_keys(v, "permission", keys);
    object_keys(v, "tools", keys);
    for section in ["agent", "mode"] {
        if let Some(entries) = obj.get(section).and_then(serde_json::Value::as_object) {
            for entry in entries.values() {
                object_keys(entry, "permission", keys);
            }
        }
    }
}

/// Strips what `serde_json` cannot read in a JSONC file: `//` line and
/// `/* */` block comments, and a trailing comma before a `}` or `]`, none
/// of it taken from inside a string. Not every JSON5 liberty (no unquoted
/// keys, no single-quoted strings): opencode's own configs do not use them.
fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut pending_comma = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                if pending_comma {
                    out.push(',');
                    pending_comma = false;
                }
                in_string = true;
                out.push(c);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                i += 2;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
            }
            ',' => {
                pending_comma = true;
                i += 1;
            }
            '}' | ']' => {
                pending_comma = false;
                out.push(c);
                i += 1;
            }
            c if c.is_whitespace() => {
                out.push(c);
                i += 1;
            }
            _ => {
                if pending_comma {
                    out.push(',');
                    pending_comma = false;
                }
                out.push(c);
                i += 1;
            }
        }
    }
    if pending_comma {
        out.push(',');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn parse(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    /// Everything is denied first, the to-do list is allowed, each tool
    /// the profile allows is allowed, and `Bash(x:*)` is the command and
    /// the command with more words.
    #[test]
    fn a_profile_becomes_deny_by_default_rules() {
        let review = crate::config::profile::Profiles::default()
            .resolve("review")
            .unwrap();
        let json = permission_json(&review.allow, &review.deny, false, &[]);
        assert!(json.starts_with(r#"{"*":"deny","#), "{json}");
        let v = parse(&json);
        assert_eq!(v["todowrite"], "allow");
        assert_eq!(v["read"], "allow");
        assert_eq!(v["list"], "allow");
        assert_eq!(v["glob"], "allow");
        assert_eq!(v["edit"], "deny");
        assert_eq!(v["bash"]["git log"], "allow");
        assert_eq!(v["bash"]["git log *"], "allow");
        assert_eq!(v["bash"]["rm -rf *"], "deny");
        assert_eq!(v["bash"]["git push *"], "deny");
        assert!(v.get("webfetch").is_none(), "{json}");
    }

    /// opencode takes the last rule that matches, so a permission's denies
    /// come after its allows, and a pattern both allowed and denied is one
    /// key, the deny, at the end.
    #[test]
    fn denies_come_last_and_win() {
        let json = permission_json(
            &strings(&["Bash", "Edit", "Bash(sudo:*)", "Read(~/.ssh/**)"]),
            &strings(&["Bash(sudo:*)", "Write", "Read(~/.ssh/**)"]),
            false,
            &[],
        );
        assert_eq!(
            json,
            r#"{"*":"deny","todoread":"allow","todowrite":"allow","bash":{"*":"allow","sudo":"deny","sudo *":"deny"},"edit":"deny","read":{"~/.ssh/**":"deny"},"list":{"~/.ssh/**":"deny"}}"#
        );
        parse(&json);
    }

    /// A tool opencode has no permission for adds nothing, allowed or
    /// denied; the `*` deny already covers it.
    #[test]
    fn unknown_tools_add_nothing() {
        let json = permission_json(
            &strings(&["mcp__x", "Foo(bar)"]),
            &strings(&["Baz"]),
            false,
            &[],
        );
        assert_eq!(
            json,
            r#"{"*":"deny","todoread":"allow","todowrite":"allow"}"#
        );
    }

    /// `unrestricted` denies nothing, so the fallback is allow: `task`, MCP
    /// tools and any other permission no Claude name maps to stay open.
    #[test]
    fn unrestricted_allows_what_it_does_not_name() {
        let open = crate::config::profile::Profiles::default()
            .resolve(crate::config::profile::UNRESTRICTED)
            .unwrap();
        let json = permission_json(&open.allow, &open.deny, true, &[]);
        assert!(json.starts_with(r#"{"*":"allow","#), "{json}");
        let v = parse(&json);
        assert!(v.get("task").is_none(), "{json}");
        assert_eq!(v["bash"], "allow");
    }

    /// A key the machine's own config sets (`never_allow`) stays denied
    /// under every profile, `unrestricted` included: its `"*"` deny is
    /// added last, so it is the one opencode's last-match-wins reading
    /// lands on, whatever the profile allowed for the same key. The
    /// always-allowed to-do list is the one exception.
    #[test]
    fn never_allow_stays_denied_under_every_profile() {
        let open = crate::config::profile::Profiles::default()
            .resolve(crate::config::profile::UNRESTRICTED)
            .unwrap();
        let json = permission_json(
            &open.allow,
            &open.deny,
            true,
            &strings(&["bash", "edit", "todowrite"]),
        );
        let v = parse(&json);
        assert_eq!(v["bash"], "deny", "{json}");
        assert_eq!(v["edit"], "deny", "{json}");
        assert_eq!(v["todowrite"], "allow", "the to-do list is never denied");
        assert_eq!(v["read"], "allow", "a key not named stays open");

        // A profile that explicitly allows a tool is overridden too, not
        // just unrestricted's unlisted fallback.
        let json = permission_json(&strings(&["Edit"]), &[], false, &strings(&["edit"]));
        assert_eq!(parse(&json)["edit"], "deny", "{json}");
    }

    /// A `"permission"` key in any of the files is a yes; the word as a
    /// value is not.
    #[test]
    fn parse_permission_keys_reads_the_opencode_config_dir() {
        let home = tempfile::tempdir().unwrap();
        let run = || read_keys(home.path());
        assert_eq!(run(), MachineDenyKeys::None);
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"model": "x", "instructions": ["permission"]}"#,
        )
        .unwrap();
        assert_eq!(run(), MachineDenyKeys::None);
        std::fs::write(
            dir.join("opencode.jsonc"),
            "{\n  \"permission\" : { \"edit\": \"ask\" }\n}",
        )
        .unwrap();
        assert_eq!(run(), MachineDenyKeys::Keys(vec!["edit".into()]));
    }

    proptest::proptest! {
        /// Any string translates without a panic, and to nothing but
        /// known permissions with at least one pattern each.
        #[test]
        fn prop_translate_never_panics(s in ".*") {
            for (perm, patterns) in translate(&s) {
                proptest::prop_assert!(!perm.is_empty());
                proptest::prop_assert!(!patterns.is_empty(), "{:?}", s);
            }
        }

        /// A pattern shaped like a Claude one, from any tool name and any
        /// argument, never panics either, nor does the whole object.
        #[test]
        fn prop_translate_takes_any_tool_and_argument(
            tool in "(Read|Edit|Bash|Task|.{0,6})",
            arg in ".*",
            deny in proptest::prelude::any::<bool>(),
        ) {
            let pattern = format!("{tool}({arg})");
            translate(&pattern);
            let list = vec![pattern];
            let (allow, deny) = if deny { (vec![], list) } else { (list, vec![]) };
            let json = permission_json(&allow, &deny, false, &[]);
            proptest::prop_assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok(), "{}", json);
        }

        /// `Bash(x:*)` is bash on `x` and on `x *`, for any `x`.
        #[test]
        fn prop_bash_prefix_is_the_command_and_the_command_with_more_words(x in ".*") {
            proptest::prop_assert_eq!(
                translate(&format!("Bash({x}:*)")),
                vec![("bash", vec![x.clone(), format!("{x} *")])]
            );
        }
    }

    /// Runs the check command with `home` as `HOME` and `managed` as
    /// opencode's managed config directory, and its managed preferences
    /// under `managed/prefs`.
    fn check(home: &std::path::Path, managed: &std::path::Path) -> String {
        check_with_path(home, managed, &std::env::var("PATH").unwrap())
    }

    /// Runs `CONFIG_READ_COMMAND` with `home` as `HOME` and parses its
    /// output, the way dispatch does with what the connector reads back.
    fn read_keys(home: &std::path::Path) -> MachineDenyKeys {
        let out = std::process::Command::new("sh")
            .args(["-c", CONFIG_READ_COMMAND])
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        parse_permission_keys(&String::from_utf8(out.stdout).unwrap())
    }

    fn check_with_path(home: &std::path::Path, managed: &std::path::Path, path: &str) -> String {
        let out = std::process::Command::new("sh")
            .args(["-c", CONFIG_CHECK_COMMAND])
            .env("HOME", home)
            .env("PATH", path)
            .env_remove("XDG_CONFIG_HOME")
            .env("OPENCODE_TEST_MANAGED_CONFIG_DIR", managed)
            .env("PASTOR_TEST_MANAGED_PREFERENCES_DIR", managed.join("prefs"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    /// On macOS the MDM preferences count, the user's and the machine's,
    /// read through `plutil`; one it cannot read is a yes. A stub `uname`
    /// says Darwin, and a stub `plutil` prints the file as its JSON.
    #[test]
    fn the_config_check_command_reads_managed_preferences_on_macos() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let stub = |name: &str, body: &str| {
            let file = bin.path().join(name);
            std::fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        stub("uname", "echo Darwin");
        stub(
            "plutil",
            r#"case "$5" in *broken*) exit 1;; esac; cat "$5""#,
        );
        let path = format!(
            "{}:{}",
            bin.path().display(),
            std::env::var("PATH").unwrap()
        );
        let run = || check_with_path(home.path(), managed.path(), &path);
        let prefs = managed.path().join("prefs");
        let user = String::from_utf8(
            std::process::Command::new("id")
                .arg("-un")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let mine = prefs.join(user.trim());
        std::fs::create_dir_all(&mine).unwrap();
        assert_eq!(run(), "no");
        let plist = prefs.join("ai.opencode.managed.plist");
        std::fs::write(&plist, r#"{"model":"x"}"#).unwrap();
        assert_eq!(run(), "no");
        std::fs::write(&plist, r#"{"permission":{"edit":"deny"}}"#).unwrap();
        assert_eq!(run(), "yes");
        std::fs::remove_file(&plist).unwrap();
        std::fs::write(
            mine.join("ai.opencode.managed.plist"),
            r#"{"tools":{"bash":false}}"#,
        )
        .unwrap();
        assert_eq!(run(), "yes");
        std::fs::write(mine.join("ai.opencode.managed.plist"), "broken").unwrap();
        stub("plutil", r#"exit 1"#);
        assert_eq!(run(), "yes");
    }

    /// opencode also reads `~/.opencode`, whatever `XDG_CONFIG_HOME` says.
    #[test]
    fn parse_permission_keys_reads_the_home_opencode_dir() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(read_keys(home.path()), MachineDenyKeys::None);
        let dir = home.path().join(".opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"permission": {"bash": "allow"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_keys(home.path()),
            MachineDenyKeys::Keys(vec!["bash".into()])
        );
    }

    /// The managed directory (`/etc/opencode`, on macOS `/Library/Application
    /// Support/opencode`) is read too; opencode's own override for it points
    /// the test at a temp dir.
    #[test]
    fn the_config_check_command_reads_the_managed_dir() {
        let home = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        std::fs::write(
            managed.path().join("opencode.jsonc"),
            "// managed\n{ \"permission\": { \"edit\": \"deny\" } }",
        )
        .unwrap();
        assert_eq!(check(home.path(), managed.path()), "yes");
    }

    /// opencode turns a legacy `tools` block into permission rules, so one
    /// counts as rules.
    #[test]
    fn parse_permission_keys_counts_a_tools_block() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"tools": {"bash": false}}"#).unwrap();
        assert_eq!(
            read_keys(home.path()),
            MachineDenyKeys::Keys(vec!["bash".into()])
        );
    }

    /// `agent.<name>.permission` and `mode.<name>.permission` count too,
    /// each key found in them added to the same set, deduplicated and
    /// sorted (`BTreeSet`); two files contribute to the one answer.
    #[test]
    fn parse_permission_keys_reads_agent_and_mode_overrides() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"permission": {"edit": "ask"}, "agent": {"build": {"permission": {"bash": "allow"}}}}"#,
        )
        .unwrap();
        let home_dir = home.path().join(".opencode");
        std::fs::create_dir_all(&home_dir).unwrap();
        std::fs::write(
            home_dir.join("opencode.json"),
            r#"{"mode": {"plan": {"permission": {"webfetch": "ask", "edit": "deny"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            read_keys(home.path()),
            MachineDenyKeys::Keys(vec!["bash".into(), "edit".into(), "webfetch".into()])
        );
    }

    /// A file pastor cannot parse once comments and trailing commas are
    /// stripped makes the whole answer `Unreadable`, even though another
    /// file read fine: pastor cannot say what the unreadable one would let
    /// through, so the task should refuse rather than run unprotected.
    #[test]
    fn parse_permission_keys_is_unreadable_on_a_bad_file() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"permission": {"edit": "ask"}"#,
        )
        .unwrap();
        assert_eq!(read_keys(home.path()), MachineDenyKeys::Unreadable);
    }

    /// JSONC's `//` and `/* */` comments and trailing commas, none of it
    /// inside a string, are gone after `strip_jsonc`; a comment-like or
    /// comma-like sequence inside a string is left alone.
    #[test]
    fn strip_jsonc_drops_comments_and_trailing_commas() {
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(
            "// leading\n{\n  \"a\": 1, // trailing\n  \"b\": /* inline */ 2,\n  \"c\": [1, 2,],\n  \"d\": \"a // not a comment, trailing\",\n}\n",
        ))
        .unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
        assert_eq!(v["c"], serde_json::json!([1, 2]));
        assert_eq!(v["d"], "a // not a comment, trailing");
    }

    /// The instructions a profiled task gets back: the repo's files by
    /// their paths, as JSON, `AGENTS.md` first.
    #[test]
    fn instructions_name_the_repo_files() {
        let files = instruction_candidates("/srv/a \"b\"/");
        assert_eq!(
            instructions_content(&files),
            r#"{"instructions":["/srv/a \"b\"/AGENTS.md","/srv/a \"b\"/CLAUDE.md"]}"#
        );
    }
}
