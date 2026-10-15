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
//! top-level permission key found there in the same object, under every
//! profile, `unrestricted` included (`permission_json`'s `never_allow`),
//! rather than refuse the task. Dispatch still refuses with
//! `OPENCODE_PERMISSIONS_CONFLICT` for what that object cannot neutralise
//! (`MachineDenyKeys::Conflict`): rules under an agent or a mode, which
//! opencode appends after the top-level ones; the managed config or, on
//! macOS, the managed preferences (`CONFIG_CHECK_COMMAND`), merged after
//! `OPENCODE_CONFIG_CONTENT`; a file it found but could not parse, or more
//! config than it reads, since it then cannot say what would get through.
//!
//! How opencode (1.19) reads what pastor sends, which the shape of
//! `permission_json` follows: `OPENCODE_PERMISSION` is deep-merged over the
//! config's `permission` as JavaScript spreads objects, so a key the
//! machine's config already has keeps its place there, with pastor's value,
//! and pastor's other keys follow in pastor's order; a legacy `tools` block
//! becomes permission keys merged under all of that (`write`, `edit` and
//! `patch` as `edit`); a `permission` that is one action is `"*"` set to it.
//! Each agent's rules are opencode's defaults (`"*": "allow"`, then its few
//! asks), then the merged top-level object in that order, then the agent's
//! own `permission` and `tools`, and the last rule matching a call wins.

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
/// cannot neutralise, or whose global config or `~/.opencode` has what
/// `MachineDenyKeys::Conflict` names.
pub const OPENCODE_PERMISSIONS_CONFLICT: &str = "opencode_permissions_conflict";

/// opencode permissions a profile always allows: the agent's own to-do list,
/// which touches nothing outside it.
const ALWAYS_ALLOWED: [&str; 2] = ["todoread", "todowrite"];

/// What opencode's own default rules ask about rather than allow (its
/// `agent.ts`, 1.19): a tool call repeated too often, and a path outside
/// the project. `unrestricted` allows them through its `"*": "allow"`
/// fallback; on a machine whose config sets keys that fallback would land
/// after them and lift their denies, so it is left out and these are allowed
/// by name instead (`permission_json`). One opencode adds later would ask,
/// and the task shows blocked: a dialog, never a rule widened.
const OPEN_ALLOWS: [&str; 2] = ["doom_loop", "external_directory"];

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
/// `"bash"`, found at the top level of the machine's own config, outside
/// any profile) that must stay denied no matter what the profile says,
/// `unrestricted` included.
///
/// opencode takes the last rule that matches, in the object's order, and
/// asks about nothing a rule denies, so the object opens with `"*": "deny"`
/// (a tool no pattern allows is refused, not asked about), or with
/// `"*": "allow"` when `open` (profile `unrestricted`, every tool: opencode
/// has tools no Claude name maps to, such as MCP ones), then the to-do
/// list, then each permission with its allows before its denies, and each
/// key of `never_allow` set to `"deny"` (`add` puts that `*` rule last in
/// the key's list, where it wins over the profile's patterns for it).
///
/// Where the object's keys land is opencode's doing, not pastor's (the
/// module doc): a key the machine's config has keeps the machine's place,
/// ahead of everything pastor adds, so a `"*": "allow"` from pastor would
/// come after the machine's keys and, matching every call, lift their
/// denies. So with `never_allow` an open profile has no fallback of its
/// own: opencode's default `"*": "allow"` stands, which comes before the
/// whole object, and what that default asks about is allowed by name
/// (`OPEN_ALLOWS`). The `"*": "deny"` of the other profiles is safe either
/// way, since it only ever denies more. A machine whose own config sets
/// `"*"` keeps its place for it, with `"deny"` from pastor under every
/// profile, as any other key of its. A permission with only `*` is written
/// as its action alone. Built by hand, since the order is the meaning and
/// serde_json sorts its maps.
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
    let machine_wildcard = never_allow.iter().any(|k| k == "*");
    let fallback = match (open, machine_wildcard, never_allow.is_empty()) {
        (false, _, _) | (true, true, _) => Some("deny"),
        (true, false, true) => Some("allow"),
        (true, false, false) => None,
    };
    if let Some(action) = fallback {
        add("*", "*".into(), action);
    }
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
    if fallback.is_none() {
        for perm in OPEN_ALLOWS {
            if !never_allow.iter().any(|k| k == perm) {
                add(perm, "*".into(), "allow");
            }
        }
    }
    for perm in never_allow {
        add(perm, "*".into(), "deny");
    }
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let entries: Vec<String> = order
        .iter()
        .map(|perm| {
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
            format!("{}:{value}", quote(perm))
        })
        .collect();
    format!("{{{}}}", entries.join(","))
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

/// The most of the machine's own opencode config `CONFIG_READ_COMMAND`
/// prints, in bytes, all files together. A probe's output is capped on the
/// head (64 KiB, `herdr::transport`), and going over it is read as the
/// machine lost; this leaves that cap room for the markers and whatever the
/// machine's rc files print, so a large config is a task's refusal
/// (`MachineDenyKeys::Conflict`), never a lost machine.
pub const CONFIG_READ_BUDGET: usize = 48 * 1024;

/// The line `CONFIG_READ_COMMAND` prints, and nothing after it, once the
/// files would exceed `CONFIG_READ_BUDGET`.
const OVERSIZED_MARKER: &str = "===oversized===";

/// The shell command that prints the content of every one of the machine's
/// own global opencode config files and `~/.opencode`'s that exists:
/// `config.json`, `opencode.json` and `opencode.jsonc` under
/// `$XDG_CONFIG_HOME/opencode` (else `~/.config/opencode`), then the same
/// three under `~/.opencode`. Each file's content is preceded by a
/// `===<path>===` line of its own so `parse_permission_keys` can tell them
/// apart; a file that is not there prints nothing. The files' sizes are
/// added up first, and the one that would take the total past
/// `CONFIG_READ_BUDGET` (49152) prints `OVERSIZED_MARKER` instead and ends
/// the output. The managed dir and macOS managed preferences are not read
/// here: `CONFIG_CHECK_COMMAND` covers those, since pastor cannot neutralise
/// what they set.
pub const CONFIG_READ_COMMAND: &str = r#"d="${XDG_CONFIG_HOME:-$HOME/.config}/opencode"; h="$HOME/.opencode"; t=0; for f in "$d/config.json" "$d/opencode.json" "$d/opencode.jsonc" "$h/config.json" "$h/opencode.json" "$h/opencode.jsonc"; do if [ -f "$f" ]; then s=$(wc -c < "$f"); t=$((t+s)); if [ "$t" -gt 49152 ]; then printf '===oversized===\n'; exit 0; fi; printf '===%s===\n' "$f"; cat "$f"; printf '\n'; fi; done"#;

/// What a profiled opencode task's env must always deny on top of its
/// profile, from the machine's own global opencode config and
/// `~/.opencode` (`CONFIG_READ_COMMAND`, read through `parse_permission_keys`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MachineDenyKeys {
    /// Nothing was found there, or it cannot be known (a `command` machine).
    #[default]
    None,
    /// Every top-level opencode permission key found, to deny under every
    /// profile (`permission_json`'s `never_allow`).
    Keys(Vec<String>),
    /// Rules there that `OPENCODE_PERMISSION` cannot neutralise, or files
    /// pastor cannot vouch for, with what was found, as a predicate on "the
    /// opencode config on <machine>": the task refuses instead of running
    /// under rules nobody chose.
    Conflict(String),
}

/// Reads `CONFIG_READ_COMMAND`'s output into `MachineDenyKeys`: splits it on
/// its `===<path>===` markers, and for each file's content that is not
/// blank, strips JSONC (`strip_jsonc`) and parses it as JSON, collecting
/// every top-level permission key (`collect_permission_keys`). A file that
/// fails to parse, one with rules under an agent or a mode, or the oversized
/// marker makes the whole answer a `Conflict`, since any of them leaves
/// pastor unable to say what the config would still let through.
pub fn parse_permission_keys(output: &str) -> MachineDenyKeys {
    let mut keys = BTreeSet::new();
    let mut current: Option<(String, String)> = None;
    let mut finish = |current: &mut Option<(String, String)>| -> Result<(), String> {
        match current.take() {
            Some((path, content)) => collect_keys_from_file(&path, &content, &mut keys),
            None => Ok(()),
        }
    };
    for line in output.split('\n') {
        if line == OVERSIZED_MARKER {
            return MachineDenyKeys::Conflict(format!(
                "is more than pastor reads ({} bytes in all)",
                CONFIG_READ_BUDGET
            ));
        }
        if line.starts_with("===") && line.ends_with("===") && line.len() > 6 {
            if let Err(why) = finish(&mut current) {
                return MachineDenyKeys::Conflict(why);
            }
            current = Some((line[3..line.len() - 3].to_string(), String::new()));
            continue;
        }
        if let Some((_, content)) = &mut current {
            content.push_str(line);
            content.push('\n');
        }
    }
    if let Err(why) = finish(&mut current) {
        return MachineDenyKeys::Conflict(why);
    }
    if keys.is_empty() {
        MachineDenyKeys::None
    } else {
        MachineDenyKeys::Keys(keys.into_iter().collect())
    }
}

/// Parses one file's content as JSONC and adds its top-level permission
/// keys to `keys`. `Err` says why not: the content is not blank but does
/// not parse as JSON once comments and trailing commas are stripped, or it
/// has rules pastor cannot neutralise (`collect_permission_keys`).
fn collect_keys_from_file(
    path: &str,
    content: &str,
    keys: &mut BTreeSet<String>,
) -> Result<(), String> {
    if content.trim().is_empty() {
        return Ok(());
    }
    let value: serde_json::Value = serde_json::from_str(&strip_jsonc(content)).map_err(|_| {
        format!(
            "has a file pastor could not parse, {path} (not JSON once comments and trailing commas \
             are stripped)"
        )
    })?;
    collect_permission_keys(&value, keys).map_err(|why| format!("has {why} in {path}"))
}

/// The permission keys `container.permission` sets, as opencode reads it:
/// an object's keys, or `*` when it is one action for every tool.
fn permission_keys(container: &serde_json::Value, keys: &mut BTreeSet<String>) {
    match container.get("permission") {
        Some(serde_json::Value::Object(map)) => keys.extend(map.keys().cloned()),
        Some(serde_json::Value::String(_)) => {
            keys.insert("*".to_string());
        }
        _ => {}
    }
}

/// The permission keys `container.tools`, the legacy block of tool names
/// to booleans, sets, as opencode turns them into rules: `write`, `edit`
/// and `patch` all mean `edit`, any other name itself.
fn tools_keys(container: &serde_json::Value, keys: &mut BTreeSet<String>) {
    if let Some(map) = container
        .get("tools")
        .and_then(serde_json::Value::as_object)
    {
        keys.extend(map.keys().map(|tool| match tool.as_str() {
            "write" | "edit" | "patch" => "edit".to_string(),
            other => other.to_string(),
        }));
    }
}

/// The opencode permission keys a parsed config file sets at the top level,
/// in `permission` and in the legacy `tools` block. Only the key names are
/// kept (`"bash"`, `"edit"`, ...), not their patterns: a key found is denied
/// outright (`permission_json`'s `never_allow`). `Err` names rules that
/// `OPENCODE_PERMISSION` cannot reach: the `permission` or `tools` of an
/// entry of `agent` or `mode`, which opencode appends to that agent's rules
/// after the top-level ones, so a deny there would be lifted by the agent's
/// own allow for the same key.
fn collect_permission_keys(
    v: &serde_json::Value,
    keys: &mut BTreeSet<String>,
) -> Result<(), String> {
    let Some(obj) = v.as_object() else {
        return Ok(());
    };
    permission_keys(v, keys);
    tools_keys(v, keys);
    for section in ["agent", "mode"] {
        let Some(entries) = obj.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, entry) in entries {
            let mut scoped = BTreeSet::new();
            permission_keys(entry, &mut scoped);
            tools_keys(entry, &mut scoped);
            if !scoped.is_empty() {
                return Err(format!(
                    "permission rules under {section}.{name} ({}), which opencode puts after the \
                     profile's",
                    scoped.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    Ok(())
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

    /// A key the machine's own config sets (`never_allow`) is `"deny"` in
    /// the object under every profile, `unrestricted` included, whatever
    /// the profile allowed for it; with keys to deny an open profile has
    /// no `"*"` of its own and allows what opencode's defaults ask about by
    /// name, and a machine `"*"` is `"deny"` like any other key of its.
    #[test]
    fn never_allow_is_denied_under_every_profile() {
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
        assert_eq!(v["todowrite"], "deny", "{json}");
        assert_eq!(v["read"], "allow", "a key not named stays open");
        assert!(v.get("*").is_none(), "{json}");
        assert_eq!(v["doom_loop"], "allow", "{json}");
        assert_eq!(v["external_directory"], "allow", "{json}");
        assert!(
            json.starts_with(r#"{"todoread":"allow","#),
            "no fallback first: {json}"
        );

        // The named allows give way to the machine too.
        let json = permission_json(
            &open.allow,
            &open.deny,
            true,
            &strings(&["external_directory"]),
        );
        assert_eq!(parse(&json)["external_directory"], "deny", "{json}");
        assert_eq!(parse(&json)["doom_loop"], "allow", "{json}");

        // A machine `"*"`: the fallback is deny, written once.
        let json = permission_json(&open.allow, &open.deny, true, &strings(&["*", "bash"]));
        assert!(json.starts_with(r#"{"*":"deny","#), "{json}");
        assert_eq!(json.matches(r#""*":"deny""#).count(), 1, "{json}");
        assert_eq!(parse(&json)["bash"], "deny", "{json}");
        assert!(parse(&json).get("doom_loop").is_none(), "{json}");

        // A profile that explicitly allows a tool is overridden too, not
        // just unrestricted's unlisted fallback, and keeps its fallback.
        let json = permission_json(&strings(&["Edit"]), &[], false, &strings(&["edit"]));
        assert_eq!(parse(&json)["edit"], "deny", "{json}");
        assert!(json.starts_with(r#"{"*":"deny","#), "{json}");
    }

    /// A JSON value with its object keys in document order, since here the
    /// order is the meaning and `serde_json::Value` sorts its maps.
    #[derive(Debug, Clone, PartialEq)]
    enum Ordered {
        Str(String),
        Obj(Vec<(String, Ordered)>),
        Other,
    }

    impl<'de> serde::Deserialize<'de> for Ordered {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = Ordered;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a JSON value")
                }
                fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Ordered, E> {
                    Ok(Ordered::Str(s.to_string()))
                }
                fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Ordered, E> {
                    Ok(Ordered::Other)
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Ordered, A::Error> {
                    let mut entries = vec![];
                    while let Some((k, v)) = map.next_entry::<String, Ordered>()? {
                        entries.push((k, v));
                    }
                    Ok(Ordered::Obj(entries))
                }
            }
            d.deserialize_any(V)
        }
    }

    /// remeda's `mergeDeep`, as opencode merges `OPENCODE_PERMISSION` over
    /// its config's `permission` (1.19): `{...destination, ...source}`, so
    /// a shared key keeps the destination's place with the source's value,
    /// recursively when both sides have an object there, and the source's
    /// other keys follow.
    fn merge_deep(destination: &Ordered, source: &Ordered) -> Ordered {
        let (Ordered::Obj(d), Ordered::Obj(s)) = (destination, source) else {
            return source.clone();
        };
        let mut out: Vec<(String, Ordered)> = d
            .iter()
            .map(|(k, dv)| match s.iter().find(|(sk, _)| sk == k) {
                Some((_, sv)) => (k.clone(), merge_deep(dv, sv)),
                None => (k.clone(), dv.clone()),
            })
            .collect();
        for (k, sv) in s {
            if !d.iter().any(|(dk, _)| dk == k) {
                out.push((k.clone(), sv.clone()));
            }
        }
        Ordered::Obj(out)
    }

    /// opencode's `Wildcard.match`: `*` anything, `?` one character, and a
    /// pattern ending in ` *` matches the words before it alone too.
    fn wildcard(input: &str, pattern: &str) -> bool {
        fn go(i: &[char], p: &[char]) -> bool {
            match (p.first(), i.first()) {
                (None, _) => i.is_empty(),
                (Some('*'), _) => go(i, &p[1..]) || (!i.is_empty() && go(&i[1..], p)),
                (Some('?'), Some(_)) => go(&i[1..], &p[1..]),
                (Some(c), Some(d)) if c == d => go(&i[1..], &p[1..]),
                _ => false,
            }
        }
        let input: Vec<char> = input.chars().collect();
        let pattern: Vec<char> = pattern.chars().collect();
        go(&input, &pattern)
            || (pattern.ends_with(&[' ', '*']) && go(&input, &pattern[..pattern.len() - 2]))
    }

    /// An agent's rules as opencode (1.19, `agent.ts` and `config.ts`)
    /// builds them for the default agent from the machine's own top-level
    /// `permission` (`machine`, JSON) and pastor's `OPENCODE_PERMISSION`:
    /// opencode's defaults first (`"*": "allow"` and the asks pastor knows
    /// of), then the merged object's rules in its order (`fromConfig`: an
    /// action alone is the `*` pattern). Each rule is (permission, pattern,
    /// action).
    fn opencode_rules(machine: &str, pastor: &str) -> Vec<(String, String, String)> {
        let machine: Ordered = serde_json::from_str(machine).unwrap();
        let pastor: Ordered = serde_json::from_str(pastor).unwrap();
        let rule = |p: &str, pat: &str, a: &str| (p.to_string(), pat.to_string(), a.to_string());
        let mut rules = vec![
            rule("*", "*", "allow"),
            rule("doom_loop", "*", "ask"),
            rule("external_directory", "*", "ask"),
            rule("question", "*", "deny"),
            rule("read", "*", "allow"),
            rule("read", "*.env", "ask"),
            rule("question", "*", "allow"),
        ];
        let Ordered::Obj(merged) = merge_deep(&machine, &pastor) else {
            panic!("not an object");
        };
        for (perm, value) in merged {
            match value {
                Ordered::Str(action) => rules.push(rule(&perm, "*", &action)),
                Ordered::Obj(patterns) => {
                    for (pattern, action) in patterns {
                        let Ordered::Str(action) = action else {
                            panic!("not an action");
                        };
                        rules.push(rule(&perm, &pattern, &action));
                    }
                }
                Ordered::Other => panic!("not a rule"),
            }
        }
        rules
    }

    /// opencode's `evaluate`: the last rule matching the call wins, `ask`
    /// when none does.
    fn evaluate(rules: &[(String, String, String)], permission: &str, pattern: &str) -> String {
        rules
            .iter()
            .rev()
            .find(|(p, pat, _)| wildcard(permission, p) && wildcard(pattern, pat))
            .map(|(_, _, action)| action.clone())
            .unwrap_or_else(|| "ask".to_string())
    }

    /// The machine's keys `parse_permission_keys` finds in `machine`, as a
    /// `permission` object in a file of its own.
    fn machine_keys(machine: &str) -> Vec<String> {
        match parse_permission_keys(&format!(
            "===/home/x/.config/opencode/opencode.json===\n{{\"permission\": {machine}}}\n"
        )) {
            MachineDenyKeys::Keys(keys) => keys,
            other => panic!("{other:?}"),
        }
    }

    /// What opencode ends up enforcing, not what pastor wrote: after its
    /// merge the machine's keys sit ahead of pastor's, so this is the
    /// reading that matters. Every key the machine's config sets is denied
    /// under every profile, for the machine's own patterns too, while the
    /// profile keeps what it allows for the other keys; `unrestricted`
    /// keeps tools no Claude name covers open, except on a machine whose
    /// config sets `"*"`, and never asks about what opencode's defaults
    /// would.
    #[test]
    fn machine_keys_stay_denied_after_opencodes_merge() {
        let profiles = crate::config::profile::Profiles::default();
        let machines = [
            r#"{"bash": "allow"}"#,
            r#"{"bash": {"git push *": "allow", "*": "ask"}, "edit": "allow"}"#,
            r#"{"webfetch": "allow", "read": {"*.env": "allow"}}"#,
            r#"{"*": "allow"}"#,
            r#"{"*": "ask", "read": "allow"}"#,
            r#"{"external_directory": {"~/ghq/**": "allow"}, "doom_loop": "allow"}"#,
        ];
        for name in ["review", "develop", crate::config::profile::UNRESTRICTED] {
            let profile = profiles.resolve(name).unwrap();
            let open = name == crate::config::profile::UNRESTRICTED;
            for machine in machines {
                let keys = machine_keys(machine);
                let pastor = permission_json(&profile.allow, &profile.deny, open, &keys);
                let rules = opencode_rules(machine, &pastor);
                let why = format!("{name} on {machine}: {pastor}");
                for key in &keys {
                    for call in ["x", "git push origin", "~/ghq/pastor/README.md", "a.env"] {
                        assert_eq!(evaluate(&rules, key, call), "deny", "{key} {call} {why}");
                    }
                }
                let wildcard = keys.iter().any(|k| k == "*");
                let has = |key: &str| keys.iter().any(|k| k == key);
                // The profile's own allows and denies for the other keys.
                if !has("read") {
                    assert_eq!(evaluate(&rules, "read", "src/main.rs"), "allow", "{why}");
                    assert_eq!(evaluate(&rules, "read", ".env"), "allow", "{why}");
                }
                if !has("bash") {
                    assert_eq!(evaluate(&rules, "bash", "git log -1"), "allow", "{why}");
                    let push = if name == "review" { "deny" } else { "allow" };
                    assert_eq!(evaluate(&rules, "bash", "git push origin"), push, "{why}");
                    let sudo = if open { "allow" } else { "deny" };
                    assert_eq!(evaluate(&rules, "bash", "sudo ls"), sudo, "{why}");
                }
                if !has("edit") {
                    let edit = if name == "review" { "deny" } else { "allow" };
                    assert_eq!(evaluate(&rules, "edit", "src/main.rs"), edit, "{why}");
                }
                // Tools no Claude name covers, and opencode's own asks.
                let unnamed = if open && !wildcard { "allow" } else { "deny" };
                assert_eq!(evaluate(&rules, "mcp_github_issue", "x"), unnamed, "{why}");
                if !has("doom_loop") {
                    assert_eq!(evaluate(&rules, "doom_loop", "x"), unnamed, "{why}");
                }
                if !has("external_directory") {
                    assert_eq!(
                        evaluate(&rules, "external_directory", "/srv"),
                        unnamed,
                        "{why}"
                    );
                }
                assert_eq!(evaluate(&rules, "todowrite", "x"), "allow", "{why}");
            }
        }
    }

    /// Without keys to deny the object is as it always was: a fallback
    /// first, `allow` only under `unrestricted`.
    #[test]
    fn no_machine_keys_keeps_the_fallback_first() {
        let open = crate::config::profile::Profiles::default()
            .resolve(crate::config::profile::UNRESTRICTED)
            .unwrap();
        let json = permission_json(&open.allow, &open.deny, true, &[]);
        assert!(
            json.starts_with(r#"{"*":"allow","todoread":"allow","#),
            "{json}"
        );
        assert!(parse(&json).get("doom_loop").is_none(), "{json}");
        let rules = opencode_rules("{}", &json);
        assert_eq!(evaluate(&rules, "doom_loop", "x"), "allow");
        assert_eq!(evaluate(&rules, "mcp_github_issue", "x"), "allow");
        assert_eq!(evaluate(&rules, "read", ".env"), "allow");
    }

    /// A legacy `tools` block is read as opencode turns it into rules:
    /// `write`, `edit` and `patch` are the `edit` permission, so a machine
    /// that turned `write` off has `edit` denied, not a `write` nobody
    /// checks; any other name is its own.
    #[test]
    fn a_tools_block_names_the_permissions_opencode_makes_of_it() {
        let out = "===/home/x/.opencode/opencode.json===\n\
                   {\"tools\": {\"write\": false, \"patch\": true, \"webfetch\": false}}\n";
        assert_eq!(
            parse_permission_keys(out),
            MachineDenyKeys::Keys(strings(&["edit", "webfetch"]))
        );
        let open = crate::config::profile::Profiles::default()
            .resolve(crate::config::profile::UNRESTRICTED)
            .unwrap();
        let json = permission_json(
            &open.allow,
            &open.deny,
            true,
            &strings(&["edit", "webfetch"]),
        );
        assert_eq!(parse(&json)["edit"], "deny", "{json}");
        assert!(parse(&json).get("write").is_none(), "{json}");
    }

    /// A `permission` that is one action is opencode's `"*"` set to it, so
    /// it counts as that key, and is denied like any other.
    #[test]
    fn a_scalar_permission_is_the_wildcard_key() {
        for action in ["allow", "ask", "deny"] {
            let out = format!(
                "===/home/x/.config/opencode/config.json===\n{{\"permission\": \"{action}\"}}\n"
            );
            assert_eq!(
                parse_permission_keys(&out),
                MachineDenyKeys::Keys(strings(&["*"])),
                "{action}"
            );
        }
        let open = crate::config::profile::Profiles::default()
            .resolve(crate::config::profile::UNRESTRICTED)
            .unwrap();
        let json = permission_json(&open.allow, &open.deny, true, &strings(&["*"]));
        let rules = opencode_rules(r#"{"*": "allow"}"#, &json);
        assert_eq!(evaluate(&rules, "mcp_github_issue", "x"), "deny", "{json}");
        assert_eq!(evaluate(&rules, "bash", "ls"), "allow", "{json}");
    }

    /// Rules under an agent or a mode are a conflict, not keys: opencode
    /// appends them to that agent's rules after the top-level ones, so a
    /// deny in `OPENCODE_PERMISSION` would not reach them. Their `tools`
    /// count the same, a scalar `permission` too; an empty object does not.
    #[test]
    fn agent_and_mode_rules_are_a_conflict() {
        let file = |body: &str| format!("===/home/x/.config/opencode/opencode.json===\n{body}\n");
        let conflict = |body: &str| match parse_permission_keys(&file(body)) {
            MachineDenyKeys::Conflict(why) => why,
            other => panic!("{body}: {other:?}"),
        };
        assert_eq!(
            conflict(
                r#"{"permission": {"edit": "ask"}, "agent": {"build": {"permission": {"bash": "allow", "edit": "allow"}}}}"#
            ),
            "has permission rules under agent.build (bash, edit), which opencode puts after the \
             profile's in /home/x/.config/opencode/opencode.json"
        );
        assert!(
            conflict(r#"{"mode": {"plan": {"tools": {"write": false}}}}"#)
                .contains("mode.plan (edit)")
        );
        assert!(conflict(r#"{"agent": {"x": {"permission": "allow"}}}"#).contains("agent.x (*)"));
        assert_eq!(
            parse_permission_keys(&file(
                r#"{"permission": {"edit": "ask"}, "agent": {"build": {"permission": {}, "tools": {}, "model": "x"}}}"#
            )),
            MachineDenyKeys::Keys(strings(&["edit"]))
        );
    }

    /// Files past `CONFIG_READ_BUDGET` in all are a conflict too, from the
    /// marker the command prints in their place, so the probe's own cap
    /// (which would read as the machine lost) is never reached.
    #[test]
    fn oversized_config_is_a_conflict_not_a_lost_machine() {
        assert_eq!(CONFIG_READ_BUDGET, 49152);
        assert!(CONFIG_READ_COMMAND.contains("-gt 49152"));
        assert!(CONFIG_READ_COMMAND.contains(OVERSIZED_MARKER));
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        let big = format!(
            "{{\"permission\": {{\"bash\": \"allow\"}}, \"pad\": \"{}\"}}",
            "x".repeat(30 * 1024)
        );
        std::fs::write(dir.join("opencode.json"), &big).unwrap();
        assert_eq!(
            read_keys(home.path()),
            MachineDenyKeys::Keys(strings(&["bash"]))
        );
        std::fs::write(dir.join("opencode.jsonc"), &big).unwrap();
        let out = read_output(home.path());
        assert!(out.len() < 40 * 1024, "{}", out.len());
        assert!(out.ends_with("===oversized===\n"), "{out}");
        assert!(matches!(
            parse_permission_keys(&out),
            MachineDenyKeys::Conflict(why) if why.contains("49152")
        ));
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
        parse_permission_keys(&read_output(home))
    }

    /// What `CONFIG_READ_COMMAND` prints with `home` as `HOME`.
    fn read_output(home: &std::path::Path) -> String {
        let out = std::process::Command::new("sh")
            .args(["-c", CONFIG_READ_COMMAND])
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
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

    /// Two files contribute to the one answer, each key once, sorted
    /// (`BTreeSet`).
    #[test]
    fn parse_permission_keys_reads_both_directories_into_one_answer() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"permission": {"edit": "ask", "bash": "allow"}}"#,
        )
        .unwrap();
        let home_dir = home.path().join(".opencode");
        std::fs::create_dir_all(&home_dir).unwrap();
        std::fs::write(
            home_dir.join("opencode.json"),
            r#"{"permission": {"webfetch": "ask", "edit": "deny"}}"#,
        )
        .unwrap();
        assert_eq!(
            read_keys(home.path()),
            MachineDenyKeys::Keys(vec!["bash".into(), "edit".into(), "webfetch".into()])
        );
    }

    /// A file pastor cannot parse once comments and trailing commas are
    /// stripped makes the whole answer a `Conflict` naming it, even though
    /// another file read fine: pastor cannot say what the unreadable one
    /// would let through, so the task should refuse rather than run
    /// unprotected.
    #[test]
    fn parse_permission_keys_conflicts_on_a_bad_file() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            r#"{"permission": {"bash": "ask"}}"#,
        )
        .unwrap();
        let bad = dir.join("opencode.json");
        std::fs::write(&bad, r#"{"permission": {"edit": "ask"}"#).unwrap();
        assert_eq!(
            read_keys(home.path()),
            MachineDenyKeys::Conflict(format!(
                "has a file pastor could not parse, {} (not JSON once comments and trailing \
                 commas are stripped)",
                bad.display()
            ))
        );
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
