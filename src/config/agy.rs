//! A permission profile for an agy agent (the Antigravity CLI). agy takes
//! no policy at launch: its rules live in its settings on the machine
//! (`permissions.allow`, `.deny`, `.ask` in `GLOBAL_SETTINGS` and
//! `SHARED_CONFIG`), and the only flag that touches them puts the session
//! in `always-proceed`, where every `ask` is approved and every `deny`
//! still holds. So a task under a profile starts with `NO_ASK` and, before
//! launch, dispatch reads those two files off the machine (`RULES_COMMAND`,
//! `parse_denies`) and checks that each pattern the task denies, in agy's
//! terms (`required_rule`), is covered by a deny there (`missing`). What is
//! not refuses the task (`AGY_PERMISSIONS_MISSING`), as does a pattern agy
//! has no rule for (`AGY_DENY_UNSUPPORTED`, at queue time). The allow
//! list cannot be passed: under the flag everything not denied runs,
//! shell writes included, and the manual says so.

/// The herdr agent kind this applies to.
pub const KIND: &str = "agy";

/// What stops agy asking: `always-proceed` for tools, and no diff review
/// before a file write.
pub const NO_ASK: [&str; 3] = ["--dangerously-skip-permissions", "--mode", "accept-edits"];

/// The code of a deny pattern agy has no rule for.
pub const AGY_DENY_UNSUPPORTED: &str = "agy_deny_unsupported";

/// The code of a profiled agy task on a machine whose agy settings do not
/// deny what the task denies, or whose settings pastor could not read.
pub const AGY_PERMISSIONS_MISSING: &str = "agy_permissions_missing";

/// The code of a profiled agy task settled on a machine whose own profile
/// does not deny everything the task's does: agy's rules are per machine.
pub const AGY_PROFILE_MISMATCH: &str = "agy_profile_mismatch";

/// agy's global settings, where the message tells the owner to add rules.
pub const GLOBAL_SETTINGS: &str = "~/.gemini/antigravity-cli/settings.json";

/// The config agy shares with the desktop app; read too, never named.
pub const SHARED_CONFIG: &str = "~/.gemini/config/config.json";

/// Does `arg` pick agy's execution mode or turn its permissions off?
pub fn is_permission_arg(arg: &str) -> bool {
    arg == "--mode" || arg.starts_with("--mode=") || arg == "--dangerously-skip-permissions"
}

/// A Claude Code pattern as the agy rule a deny of it requires. `None` for
/// a tool agy has no rule for (`Glob`, `Grep`, `WebSearch`, `Task`, an
/// unknown one) and for a command with no words, which agy matches to
/// nothing. A `Bash(x)` with no `:*` is exact in Claude and a prefix in
/// agy; broader is safe for a deny. A path's trailing `/**` or `/*` goes,
/// since an agy directory rule covers what is under it.
pub fn required_rule(pattern: &str) -> Option<String> {
    let (tool, arg) = match pattern.split_once('(') {
        Some((tool, rest)) => (tool, Some(rest.strip_suffix(')').unwrap_or(rest))),
        None => (pattern, None),
    };
    let path = |arg: Option<&str>| {
        let p = arg.unwrap_or("*");
        let p = p
            .strip_suffix("/**")
            .or_else(|| p.strip_suffix("/*"))
            .unwrap_or(p);
        if p.is_empty() {
            "*".to_string()
        } else {
            p.to_string()
        }
    };
    Some(match tool {
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => format!("write_file({})", path(arg)),
        "Read" => format!("read_file({})", path(arg)),
        "Bash" => match arg {
            None => "command(*)".to_string(),
            Some(a) => {
                let words = a.strip_suffix(":*").unwrap_or(a).trim();
                if words.is_empty() {
                    return None;
                }
                format!("command({words})")
            }
        },
        "WebFetch" => match arg {
            None => "read_url(*)".to_string(),
            Some(a) => format!("read_url({})", a.strip_prefix("domain:")?),
        },
        _ => {
            let rest = tool.strip_prefix("mcp__")?;
            if arg.is_some() || rest.is_empty() {
                return None;
            }
            match rest.split_once("__") {
                Some((server, t)) if !server.is_empty() && !t.is_empty() => {
                    format!("mcp({server}/{t})")
                }
                None => format!("mcp({rest}/*)"),
                _ => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each row of the spec's translation table, and the shapes agy
    /// cannot take.
    #[test]
    fn claude_patterns_become_the_agy_rules_a_deny_requires() {
        for (pattern, rule) in [
            ("Edit", "write_file(*)"),
            ("Write", "write_file(*)"),
            ("NotebookEdit", "write_file(*)"),
            ("MultiEdit", "write_file(*)"),
            ("Edit(/srv/app/src/**)", "write_file(/srv/app/src)"),
            ("Write(~/.ssh/*)", "write_file(~/.ssh)"),
            ("Read", "read_file(*)"),
            ("Read(~/.ssh/**)", "read_file(~/.ssh)"),
            ("Bash", "command(*)"),
            ("Bash(git push:*)", "command(git push)"),
            ("Bash(git push --force:*)", "command(git push --force)"),
            ("Bash(rm -rf:*)", "command(rm -rf)"),
            ("Bash(sudo)", "command(sudo)"),
            ("WebFetch", "read_url(*)"),
            ("WebFetch(domain:example.com)", "read_url(example.com)"),
            ("mcp__linter__lint", "mcp(linter/lint)"),
            ("mcp__linter", "mcp(linter/*)"),
        ] {
            assert_eq!(required_rule(pattern).as_deref(), Some(rule), "{pattern}");
        }
        for pattern in [
            "Glob",
            "Grep",
            "WebSearch",
            "Task",
            "Foo",
            "Bash(:*)",
            "Bash()",
            "Bash( )",
            "WebFetch(example.com)",
            "mcp__",
            "mcp____x",
            "mcp__a__b(c)",
        ] {
            assert_eq!(required_rule(pattern), None, "{pattern}");
        }
    }

    #[test]
    fn mode_and_skip_flags_are_permission_args() {
        assert!(is_permission_arg("--mode"));
        assert!(is_permission_arg("--mode=plan"));
        assert!(is_permission_arg("--dangerously-skip-permissions"));
        assert!(!is_permission_arg("--sandbox"));
        assert!(!is_permission_arg("--model"));
    }

    proptest::proptest! {
        /// Any string translates without a panic, to a rule of a known
        /// action with a non-empty target.
        #[test]
        fn prop_required_rule_never_panics(s in ".*") {
            if let Some(rule) = required_rule(&s) {
                let (action, target) = rule.split_once('(').unwrap();
                proptest::prop_assert!(
                    ["write_file", "read_file", "command", "read_url", "mcp"].contains(&action),
                    "{rule}"
                );
                proptest::prop_assert!(target.ends_with(')') && target.len() > 1, "{rule}");
            }
        }
    }
}
