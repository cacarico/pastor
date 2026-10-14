//! A permission profile for an agy agent. agy has no flag or variable for
//! permission rules: they live in its settings on the machine, as
//! `action(target)` strings under `permissions.deny` (and `allow`, `ask`).
//! A profile's deny list, in Claude Code's patterns, is translated to the
//! agy rules it requires (`required_rule`), and a rule counts as there when
//! a deny in the settings covers it (`covers`, `missing`). The agy
//! provisioner (`provision::agy`) writes what is missing.
//!
//! The names and meaning are those of
//! `docs/superpowers/specs/2026-10-15-agy-profiles-design.md` (branch
//! `feat/agent-profiles`), which adds the launch flags and the check before
//! launch to this module.

/// The herdr agent kind this applies to.
pub const KIND: &str = "agy";

/// The code of a profiled agy task whose machine's agy settings do not deny
/// what its profile denies, or cannot be read or written.
pub const AGY_PERMISSIONS_MISSING: &str = "agy_permissions_missing";

/// The agy rule a Claude Code deny pattern requires, `None` for a pattern
/// agy has no rule for (`Glob`, `Grep`, `WebSearch`, `Task`, an unknown
/// tool). `Bash(<words>:*)` and `Bash(<words>)` are both `command(<words>)`:
/// agy's command rule is a word prefix, and a broader deny is safe. A path's
/// trailing `/**` or `/*` is dropped, since an agy directory rule covers
/// what is under it; a leading `~` passes as written.
pub fn required_rule(pattern: &str) -> Option<String> {
    if let Some(mcp) = pattern.strip_prefix("mcp__") {
        return match mcp.split_once("__") {
            Some((server, tool)) if !server.is_empty() && !tool.is_empty() => {
                Some(format!("mcp({server}/{tool})"))
            }
            None if !mcp.is_empty() => Some(format!("mcp({mcp}/*)")),
            _ => None,
        };
    }
    let (tool, arg) = match pattern.split_once('(') {
        Some((tool, rest)) => (tool, Some(rest.strip_suffix(')')?)),
        None => (pattern, None),
    };
    let rule = |action: &str, target: &str| Some(format!("{action}({target})"));
    match (tool, arg) {
        ("Edit" | "Write" | "NotebookEdit" | "MultiEdit", None) => rule("write_file", "*"),
        ("Edit" | "Write" | "NotebookEdit" | "MultiEdit", Some(p)) => rule("write_file", &path(p)?),
        ("Read", None) => rule("read_file", "*"),
        ("Read", Some(p)) => rule("read_file", &path(p)?),
        ("Bash", None) => rule("command", "*"),
        ("Bash", Some(arg)) => {
            let words = arg.strip_suffix(":*").unwrap_or(arg).trim();
            if words.is_empty() {
                return None;
            }
            rule("command", words)
        }
        ("WebFetch", None) => rule("read_url", "*"),
        ("WebFetch", Some(arg)) => {
            let host = arg.strip_prefix("domain:")?;
            (!host.is_empty()).then(|| format!("read_url({host})"))
        }
        _ => None,
    }
}

/// A path as an agy rule takes it: a trailing `/**` or `/*` dropped; one
/// that is then empty (`/**`) is everything.
fn path(p: &str) -> Option<String> {
    if p.is_empty() {
        return None;
    }
    let p = p
        .strip_suffix("/**")
        .or_else(|| p.strip_suffix("/*"))
        .unwrap_or(p);
    Some(if p.is_empty() { "*".into() } else { p.into() })
}

/// `action(target)` split, `None` when the rule is not of that shape.
fn split(rule: &str) -> Option<(&str, &str)> {
    let (action, rest) = rule.split_once('(')?;
    Some((action, rest.strip_suffix(')')?))
}

/// Does the deny rule `have` cover the required rule `want`? The same rule;
/// the action's `*`; for `command`, a rule whose words lead `want`'s; for
/// `write_file` and `read_file`, the same path or a directory above it, and
/// a `read_file` deny for a `write_file` rule (agy's denying a read also
/// denies the write). A `regex:` rule covers nothing other than itself:
/// pastor does not evaluate regular expressions.
pub fn covers(have: &str, want: &str) -> bool {
    if have.trim() == want.trim() {
        return true;
    }
    let (Some((h_action, h_target)), Some((w_action, w_target))) = (split(have), split(want))
    else {
        return false;
    };
    let (h_target, w_target) = (h_target.trim(), w_target.trim());
    if h_target.starts_with("regex:") {
        return false;
    }
    let same_action = h_action == w_action || (h_action == "read_file" && w_action == "write_file");
    if !same_action {
        return false;
    }
    if h_target == "*" {
        return true;
    }
    match w_action {
        "command" => {
            let h: Vec<&str> = h_target.split_whitespace().collect();
            let w: Vec<&str> = w_target.split_whitespace().collect();
            !h.is_empty() && w.starts_with(&h)
        }
        "write_file" | "read_file" => {
            let h = h_target.trim_end_matches('/');
            let w = w_target.trim_end_matches('/');
            w == h
                || (!h.is_empty() && w.starts_with(&format!("{h}/")))
                || (h.is_empty() && w.starts_with('/'))
        }
        _ => h_target == w_target,
    }
}

/// The rules of `required` that no rule of `denies` covers, in `required`'s
/// order.
pub fn missing(required: &[String], denies: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|want| !denies.iter().any(|have| covers(have, want)))
        .cloned()
        .collect()
}

/// The agy rules a deny list requires, in its order, once each: what the
/// provisioner makes sure the machine's settings deny. Patterns agy has no
/// rule for are left out.
pub fn required_rules(deny: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for rule in deny.iter().filter_map(|p| required_rule(p)) {
        if !out.contains(&rule) {
            out.push(rule);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// Each row of the design's translation table.
    #[test]
    fn claude_patterns_translate_to_agy_rules() {
        let cases = [
            ("Edit", Some("write_file(*)")),
            ("Write", Some("write_file(*)")),
            ("NotebookEdit", Some("write_file(*)")),
            ("MultiEdit", Some("write_file(*)")),
            ("Edit(/etc/**)", Some("write_file(/etc)")),
            ("Write(~/.ssh/*)", Some("write_file(~/.ssh)")),
            ("Edit(src/main.rs)", Some("write_file(src/main.rs)")),
            ("Read", Some("read_file(*)")),
            ("Read(~/.ssh/**)", Some("read_file(~/.ssh)")),
            ("Bash", Some("command(*)")),
            ("Bash(git push:*)", Some("command(git push)")),
            ("Bash(rm -rf)", Some("command(rm -rf)")),
            ("WebFetch", Some("read_url(*)")),
            (
                "WebFetch(domain:example.com)",
                Some("read_url(example.com)"),
            ),
            (
                "mcp__github__create_issue",
                Some("mcp(github/create_issue)"),
            ),
            ("mcp__github", Some("mcp(github/*)")),
            ("Glob", None),
            ("Grep", None),
            ("WebSearch", None),
            ("Task", None),
            ("Frobnicate", None),
            ("Bash(:*)", None),
            ("WebFetch(example.com)", None),
            ("Edit(", None),
        ];
        for (pattern, want) in cases {
            assert_eq!(required_rule(pattern).as_deref(), want, "{pattern}");
        }
    }

    #[test]
    fn a_deny_covers_the_same_rule_and_the_actions_star() {
        assert!(covers("command(git push)", "command(git push)"));
        assert!(covers("command(*)", "command(git push)"));
        assert!(covers("write_file(*)", "write_file(/etc)"));
        assert!(!covers("command(*)", "write_file(*)"));
        assert!(!covers("write_file(/etc)", "write_file(*)"));
    }

    #[test]
    fn a_command_deny_covers_more_words() {
        assert!(covers("command(git push)", "command(git push --force)"));
        assert!(covers("command(git   push)", "command(git push --force)"));
        assert!(!covers("command(git push --force)", "command(git push)"));
        assert!(!covers("command(git pu)", "command(git push)"));
    }

    #[test]
    fn a_path_deny_covers_what_is_under_it() {
        assert!(covers("write_file(/etc/)", "write_file(/etc/passwd)"));
        assert!(covers("write_file(/etc)", "write_file(/etc)"));
        assert!(!covers("write_file(/et)", "write_file(/etc)"));
        assert!(covers("read_file(~/.ssh)", "write_file(~/.ssh/id)"));
        assert!(!covers("write_file(~/.ssh)", "read_file(~/.ssh)"));
        assert!(covers("read_file(*)", "write_file(*)"));
    }

    #[test]
    fn a_regex_covers_nothing() {
        assert!(!covers("command(regex:.*)", "command(git push)"));
        assert!(!covers("allow", "command(git push)"));
    }

    /// The built-in profiles' denies, against a settings file that holds
    /// them, and one without `git push`.
    #[test]
    fn the_built_in_denies_are_found_missing_or_covered() {
        let profiles = crate::config::profile::Profiles::default();
        for name in ["review", "develop"] {
            let required = required_rules(&profiles.resolve(name).unwrap().deny);
            assert!(!required.is_empty(), "{name}");
            assert!(missing(&required, &required).is_empty(), "{name}");
            let without: Vec<String> = required
                .iter()
                .filter(|r| !r.starts_with("command(git push"))
                .cloned()
                .collect();
            let gone = missing(&required, &without);
            assert!(!gone.is_empty(), "{name}: {required:?}");
            assert!(
                gone.iter().all(|r| r.starts_with("command(git push")),
                "{gone:?}"
            );
        }
        let unrestricted = profiles
            .resolve(crate::config::profile::UNRESTRICTED)
            .unwrap();
        assert!(required_rules(&unrestricted.deny).is_empty());
    }

    #[test]
    fn required_rules_keep_order_and_drop_repeats_and_untranslatable() {
        assert_eq!(
            required_rules(&strings(&[
                "Edit",
                "Glob",
                "Write",
                "Bash(sudo:*)",
                "Bash(sudo)"
            ])),
            strings(&["write_file(*)", "command(sudo)"])
        );
    }

    proptest::proptest! {
        #[test]
        fn prop_translation_never_panics(s in ".*") {
            if let Some(rule) = required_rule(&s) {
                proptest::prop_assert!(split(&rule).is_some(), "{}", rule);
            }
        }

        #[test]
        fn prop_a_rule_covers_itself_and_star_covers_its_action(
            action in "(command|write_file|read_file|read_url|mcp)",
            target in "[a-z/ ._~-]{1,12}",
        ) {
            let rule = format!("{action}({target})");
            proptest::prop_assert!(covers(&rule, &rule));
            let star = format!("{action}(*)");
            proptest::prop_assert!(covers(&star, &rule));
            proptest::prop_assert!(missing(std::slice::from_ref(&rule), std::slice::from_ref(&rule)).is_empty());
        }
    }
}
