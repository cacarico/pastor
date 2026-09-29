//! A permission profile for an opencode agent. opencode has no flags for
//! tool lists; it reads its permission rules from its config, and from
//! `OPENCODE_PERMISSION`, a JSON object merged over it. So a task under a
//! profile gets the profile's lists (in Claude Code's patterns, which the
//! profiles are written in) translated into that object, and its pane gets
//! it with the variables that point opencode at another config emptied
//! and the repo's own config turned off (`Agents::launch`), the repo's
//! instruction files passed back by path (`instructions_content`). Dispatch
//! refuses a machine whose own opencode config
//! already has permission rules (`OPENCODE_PERMISSIONS_CONFLICT`), since
//! opencode would merge the two.

use std::collections::BTreeMap;

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

/// The repo files opencode reads as instructions, which a profiled task
/// gets back by path.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// `OPENCODE_CONFIG_CONTENT` for a profiled task working in `dir`: its
/// `AGENTS.md` and `CLAUDE.md` as `instructions`. By absolute path, since
/// with the project config off opencode looks for a relative one in its
/// global config dir; a file that is not there matches nothing.
pub fn instructions_content(dir: &str) -> String {
    let dir = dir.trim_end_matches('/');
    let files: Vec<String> = INSTRUCTION_FILES
        .iter()
        .map(|f| format!("{dir}/{f}"))
        .collect();
    serde_json::json!({ "instructions": files }).to_string()
}

/// The code of a profiled opencode task sent to a machine whose own opencode
/// config has permission rules.
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
/// already put the profile's first and dropped what is denied from allow).
///
/// opencode takes the last rule that matches, in the object's order, and
/// asks about nothing a rule denies, so the object opens with `"*": "deny"`
/// (a tool no pattern allows is refused, not asked about), or with
/// `"*": "allow"` when `open` (profile `unrestricted`, every tool: opencode
/// has tools no Claude name maps to, such as MCP ones), then each
/// permission with its allows before its denies. A permission with only
/// `*` is written as its action alone. Built by hand, since the order is
/// the meaning and serde_json sorts its maps.
pub fn permission_json(allow: &[String], deny: &[String], open: bool) -> String {
    let mut order: Vec<&'static str> = vec![];
    let mut rules: BTreeMap<&'static str, Vec<(String, &'static str)>> = BTreeMap::new();
    let mut add = |perm: &'static str, pattern: String, action: &'static str| {
        let list = rules.entry(perm).or_insert_with(|| {
            order.push(perm);
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
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let fallback = if open { "allow" } else { "deny" };
    let mut out = format!("{{{}:{}", quote("*"), quote(fallback));
    for perm in order {
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

/// The shell command that answers `yes` when a config opencode reads
/// besides the project's sets permission rules, `no` otherwise: its config
/// dir (`$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode`) and
/// `~/.opencode`, each's `config.json`, `opencode.json` and
/// `opencode.jsonc`, and the managed dir (`/etc/opencode`, on macOS
/// `/Library/Application Support/opencode`, or opencode's own
/// `OPENCODE_TEST_MANAGED_CONFIG_DIR`). Any `"permission"` key counts, top
/// level or under an agent, and a `"tools"` one, which opencode turns into
/// rules; one in a comment too: a false yes refuses a task with the reason,
/// a false no would run it under rules nobody chose. The project's config
/// is off for a profiled task (`DISABLE_PROJECT_CONFIG_ENV`).
pub const CONFIG_CHECK_COMMAND: &str = r#"d="${XDG_CONFIG_HOME:-$HOME/.config}/opencode"; h="$HOME/.opencode"; m="$OPENCODE_TEST_MANAGED_CONFIG_DIR"; if [ -z "$m" ]; then if [ "$(uname -s)" = Darwin ]; then m="/Library/Application Support/opencode"; else m=/etc/opencode; fi; fi; if cat "$d/config.json" "$d/opencode.json" "$d/opencode.jsonc" "$h/config.json" "$h/opencode.json" "$h/opencode.jsonc" "$m/opencode.json" "$m/opencode.jsonc" 2>/dev/null | grep -Eq '"(permission|tools)"[[:space:]]*:'; then printf yes; else printf no; fi"#;

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
        let json = permission_json(&review.allow, &review.deny, false);
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
        let json = permission_json(&strings(&["mcp__x", "Foo(bar)"]), &strings(&["Baz"]), false);
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
        let json = permission_json(&open.allow, &open.deny, true);
        assert!(json.starts_with(r#"{"*":"allow","#), "{json}");
        let v = parse(&json);
        assert!(v.get("task").is_none(), "{json}");
        assert_eq!(v["bash"], "allow");
    }

    /// A `"permission"` key in any of the files is a yes; the word as a
    /// value is not.
    #[test]
    fn the_config_check_command_reads_the_opencode_config_dir() {
        let home = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let run = || check(home.path(), managed.path());
        assert_eq!(run(), "no");
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"model": "x", "instructions": ["permission"]}"#,
        )
        .unwrap();
        assert_eq!(run(), "no");
        std::fs::write(
            dir.join("opencode.jsonc"),
            "{\n  \"permission\" : { \"edit\": \"ask\" }\n}",
        )
        .unwrap();
        assert_eq!(run(), "yes");
    }

    /// Runs the check command with `home` as `HOME` and `managed` as
    /// opencode's managed config directory.
    fn check(home: &std::path::Path, managed: &std::path::Path) -> String {
        let out = std::process::Command::new("sh")
            .args(["-c", CONFIG_CHECK_COMMAND])
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env("OPENCODE_TEST_MANAGED_CONFIG_DIR", managed)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    /// opencode also reads `~/.opencode`, whatever `XDG_CONFIG_HOME` says.
    #[test]
    fn the_config_check_command_reads_the_home_opencode_dir() {
        let home = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        assert_eq!(check(home.path(), managed.path()), "no");
        let dir = home.path().join(".opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"permission": {"bash": "allow"}}"#,
        )
        .unwrap();
        assert_eq!(check(home.path(), managed.path()), "yes");
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
    fn the_config_check_command_counts_a_tools_block() {
        let home = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let dir = home.path().join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"tools": {"bash": false}}"#).unwrap();
        assert_eq!(check(home.path(), managed.path()), "yes");
    }

    /// The instructions a profiled task gets back: the repo's files by
    /// their paths, as JSON.
    #[test]
    fn instructions_name_the_repo_files() {
        assert_eq!(
            instructions_content("/srv/a \"b\""),
            r#"{"instructions":["/srv/a \"b\"/AGENTS.md","/srv/a \"b\"/CLAUDE.md"]}"#
        );
    }
}
