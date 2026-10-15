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

/// Does `arg` pick agy's execution mode or turn its permissions off? agy
/// parses flags as Go does: one dash reads as two, and a bool flag takes
/// `=true` or `=false`.
pub fn is_permission_arg(arg: &str) -> bool {
    let Some(name) = arg
        .strip_prefix("--")
        .or_else(|| arg.strip_prefix('-'))
        .filter(|n| !n.starts_with('-'))
    else {
        return false;
    };
    name == "mode"
        || name.starts_with("mode=")
        || name == "dangerously-skip-permissions"
        || name.starts_with("dangerously-skip-permissions=")
}

/// A Claude Code pattern as the agy rule a deny of it requires. `None` for
/// a tool agy has no rule for (`Glob`, `Grep`, `WebSearch`, `Task`, an
/// unknown one) and for a command with no words, which agy matches to
/// nothing. A `Bash(x)` with no `:*` is exact in Claude and a prefix in
/// agy; broader is safe for a deny. A path's trailing `/**` or `/*` goes,
/// since an agy directory rule covers what is under it; a glob anywhere
/// else, or in a command's words, is `None`: agy has no syntax for it, so
/// a rule written that way would match nothing and still read as covered.
pub fn required_rule(pattern: &str) -> Option<String> {
    let (tool, arg) = match pattern.split_once('(') {
        Some((tool, rest)) => (tool, Some(rest.strip_suffix(')').unwrap_or(rest))),
        None => (pattern, None),
    };
    let path = |arg: Option<&str>| -> Option<String> {
        let p = arg.unwrap_or("*");
        let p = p
            .strip_suffix("/**")
            .or_else(|| p.strip_suffix("/*"))
            .unwrap_or(p);
        if p.is_empty() || p == "*" {
            Some("*".to_string())
        } else if p.contains(['*', '?', '[']) {
            None
        } else {
            Some(p.to_string())
        }
    };
    Some(match tool {
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => format!("write_file({})", path(arg)?),
        "Read" => format!("read_file({})", path(arg)?),
        "Bash" => match arg {
            None => "command(*)".to_string(),
            Some(a) => {
                let words = a.strip_suffix(":*").unwrap_or(a).trim();
                if words.is_empty() {
                    return None;
                }
                if words != "*" && words.contains('*') {
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

/// A rule's action and target: `command(git push)` is `("command", "git
/// push")`. `None` for anything else.
fn parts(rule: &str) -> Option<(&str, &str)> {
    let (action, rest) = rule.split_once('(')?;
    let target = rest.strip_suffix(')')?;
    (!action.is_empty() && !target.is_empty()).then_some((action, target))
}

/// Does `deny`, a rule from agy's settings, cover `required`, a rule a
/// profile needs there? The same rule, the action's `*`, a command whose
/// words lead the required words, a path that is the required path or a
/// directory above it on component boundaries (a `read_file` deny of
/// such a path blocks `write_file` too, agy's implicit rule), a host that
/// is the required host or one above it, an MCP server's `*` for its
/// tools. A `regex:` command covers nothing: pastor does not evaluate it.
/// A relative path covers only the identical path.
pub fn covers(deny: &str, required: &str) -> bool {
    let (Some((da, dt)), Some((ra, rt))) = (parts(deny), parts(required)) else {
        return false;
    };
    let implicit = da == "read_file" && ra == "write_file";
    if da != ra && !implicit {
        return false;
    }
    if dt == "*" {
        return true;
    }
    if rt == "*" {
        return false;
    }
    match ra {
        "command" => {
            if dt.starts_with("regex:") {
                return false;
            }
            let dw: Vec<&str> = dt.split_whitespace().collect();
            let rw: Vec<&str> = rt.split_whitespace().collect();
            !dw.is_empty() && rw.starts_with(&dw)
        }
        "write_file" | "read_file" => {
            let dp = dt.trim_end_matches('/');
            let rp = rt.trim_end_matches('/');
            if dp.starts_with('/') && rp.starts_with('/') {
                std::path::Path::new(rp).starts_with(std::path::Path::new(dp))
            } else {
                dp == rp
            }
        }
        "read_url" => rt == dt || rt.strip_suffix(dt).is_some_and(|h| h.ends_with('.')),
        "mcp" => {
            rt == dt
                || dt
                    .strip_suffix("/*")
                    .is_some_and(|s| rt.strip_prefix(s).is_some_and(|t| t.starts_with('/')))
        }
        _ => rt == dt,
    }
}

/// The rules in `required` no rule in `denies` covers, in order, once each.
pub fn missing(required: &[String], denies: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in required {
        if !out.contains(r) && !denies.iter().any(|d| covers(d, r)) {
            out.push(r.clone());
        }
    }
    out
}

/// The patterns of `task_deny` not in `own_deny`, as written: a task of
/// kind agy may run only on a machine whose own profile denies everything
/// the task does, since agy's rules are per machine (`AGY_PROFILE_MISMATCH`).
pub fn not_denied_by(task_deny: &[String], own_deny: &[String]) -> Vec<String> {
    task_deny
        .iter()
        .filter(|p| !own_deny.contains(p))
        .cloned()
        .collect()
}

/// The marker line before each file the probe prints, and the last line.
const MARKER: &str = "==== ";
const END: &str = "end";

/// The shell command that prints agy's global settings and shared config
/// when they exist, each after a `==== <path>` line, then `==== end` so a
/// cut-off answer can be told from one with no files. Project files under
/// `~/.gemini/config/projects/` are left out: agy applies only the one
/// matching its active workspace, which pastor cannot tell. Each file is
/// cut at 30000 bytes, so two stay under the probe's 64 KiB limit and the
/// probe never fails as a transport error over a fat file: a cut file is
/// not JSON, and fails the task on the safe side.
pub const RULES_COMMAND: &str = r#"for f in "$HOME/.gemini/antigravity-cli/settings.json" "$HOME/.gemini/config/config.json"; do if [ -f "$f" ]; then printf '==== %s\n' "$f"; head -c 30000 "$f"; printf '\n'; fi; done; printf '==== end\n'"#;

/// The `permissions.deny` strings of every file in the probe's `output`,
/// in file order, or the path of the first file that is not JSON. Lines
/// before the first marker are rc-file noise and skipped; an output not
/// ending in `==== end` was cut off and is an error. A `permissions` that
/// is not an object, a `deny` that is not an array, and an entry that is
/// not a string add nothing.
pub fn parse_denies(output: &str) -> Result<Vec<String>, String> {
    let mut sections: Vec<(&str, String)> = Vec::new();
    let mut ended = false;
    for line in output.lines() {
        if let Some(path) = line.strip_prefix(MARKER) {
            if path == END {
                ended = true;
                break;
            }
            sections.push((path, String::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if !ended {
        return Err("the answer is incomplete: it does not end with `==== end`".into());
    }
    let mut denies = Vec::new();
    for (path, body) in sections {
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("{path} is not JSON ({e})"))?;
        if let Some(list) = json
            .get("permissions")
            .and_then(|p| p.get("deny"))
            .and_then(|d| d.as_array())
        {
            denies.extend(list.iter().filter_map(|v| v.as_str()).map(str::to_string));
        }
    }
    Ok(denies)
}

/// What the probe prints on a machine whose global settings deny `denies`,
/// for the fake herdr and tests.
#[cfg(any(test, feature = "fake-herdr"))]
pub fn fake_output(denies: &[&str]) -> String {
    format!(
        "{MARKER}/home/fake/.gemini/antigravity-cli/settings.json\n{}\n{MARKER}{END}\n",
        serde_json::json!({ "permissions": { "deny": denies } })
    )
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
            ("Bash(*)", "command(*)"),
            ("Edit(*)", "write_file(*)"),
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
            "Edit(/srv/app/*.rs)",
            "Edit(**/*.md)",
            "Read(/srv/[ab]/**)",
            "Write(/srv/?)",
            "Bash(git * main)",
            "Bash(rm -rf *:*)",
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
        // Go's flag parser takes one dash as two, and a bool with a value.
        for arg in [
            "-mode",
            "-mode=plan",
            "-dangerously-skip-permissions",
            "--dangerously-skip-permissions=true",
            "-dangerously-skip-permissions=false",
        ] {
            assert!(is_permission_arg(arg), "{arg}");
        }
        assert!(!is_permission_arg("--sandbox"));
        assert!(!is_permission_arg("---mode"));
        assert!(!is_permission_arg("mode"));
        assert!(!is_permission_arg("--model"));
    }

    #[test]
    fn a_deny_covers_the_same_rule_the_star_and_what_agy_treats_as_under_it() {
        for (deny, required) in [
            ("command(git push)", "command(git push)"),
            ("command(git push)", "command(git push --force)"),
            ("command(git)", "command(git push)"),
            ("command(*)", "command(sudo)"),
            ("write_file(*)", "write_file(/srv/app)"),
            ("write_file(/srv/app)", "write_file(/srv/app/src)"),
            ("write_file(/srv/app/)", "write_file(/srv/app/src)"),
            ("read_file(/srv/app)", "write_file(/srv/app/src)"),
            ("read_file(*)", "write_file(/x)"),
            ("write_file(src/)", "write_file(src)"),
            ("read_url(example.com)", "read_url(api.example.com)"),
            ("read_url(*)", "read_url(example.com)"),
            ("mcp(linter/*)", "mcp(linter/lint)"),
            ("mcp(*)", "mcp(linter/lint)"),
        ] {
            assert!(covers(deny, required), "{deny} should cover {required}");
        }
        for (deny, required) in [
            ("command(git push --force)", "command(git push)"),
            ("command(git pushing)", "command(git push)"),
            ("command(regex:git .*)", "command(git push)"),
            ("command(sudo)", "command(*)"),
            ("write_file(/srv/app)", "write_file(/srv/application)"),
            ("write_file(/srv/app)", "write_file(*)"),
            ("write_file(src)", "write_file(/srv/app/src)"),
            ("write_file(/srv/app)", "read_file(/srv/app)"),
            ("read_url(ample.com)", "read_url(example.com)"),
            ("mcp(linter/lint)", "mcp(linter/*)"),
            ("unsandboxed(sudo)", "command(sudo)"),
            ("command(sudo", "command(sudo)"),
            ("", "command(sudo)"),
        ] {
            assert!(!covers(deny, required), "{deny} must not cover {required}");
        }
    }

    /// The built-in profiles' denies against a settings file that holds
    /// them and one that lacks `command(git push)`; what is missing is
    /// listed in the order required, once each.
    #[test]
    fn missing_lists_the_required_rules_no_deny_covers() {
        let strings = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let review = crate::config::profile::Profiles::default()
            .resolve("review")
            .unwrap();
        let required: Vec<String> = review
            .deny
            .iter()
            .filter_map(|p| required_rule(p))
            .collect();
        assert_eq!(
            required,
            strings(&[
                "write_file(*)",
                "write_file(*)",
                "write_file(*)",
                "command(git push)",
                "command(rm -rf)",
                "command(sudo)",
                "command(git push --force)"
            ])
        );
        let full = strings(&[
            "write_file(*)",
            "command(git push)",
            "command(rm -rf)",
            "command(sudo)",
        ]);
        assert!(missing(&required, &full).is_empty());
        let short = strings(&[
            "write_file(*)",
            "command(rm -rf)",
            "command(sudo)",
            "command(git push --force)",
        ]);
        assert_eq!(missing(&required, &short), strings(&["command(git push)"]));
        assert_eq!(
            missing(&required, &[]),
            strings(&[
                "write_file(*)",
                "command(git push)",
                "command(rm -rf)",
                "command(sudo)",
                "command(git push --force)"
            ])
        );
    }

    /// A task's deny patterns not in the machine's own profile's, as
    /// written (Claude patterns, string equality).
    #[test]
    fn not_denied_by_compares_claude_patterns() {
        let strings = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let own = strings(&["Edit", "Bash(sudo:*)"]);
        assert_eq!(
            not_denied_by(
                &strings(&["Edit", "Bash(git push:*)", "Bash(sudo:*)"]),
                &own
            ),
            strings(&["Bash(git push:*)"])
        );
        assert!(not_denied_by(&[], &own).is_empty());
    }

    proptest::proptest! {
        /// Covering is reflexive for any well-formed rule, and the action's
        /// `*` covers every rule of that action.
        #[test]
        fn prop_a_rule_covers_itself_and_star_covers_all(
            action in "(command|write_file|read_file|read_url|mcp)",
            target in "[a-zA-Z0-9_./-]{1,20}",
        ) {
            let rule = format!("{action}({target})");
            proptest::prop_assert!(covers(&rule, &rule), "{rule}");
            proptest::prop_assert!(covers(&format!("{action}(*)"), &rule), "{rule}");
        }

        /// Any two strings compare without a panic.
        #[test]
        fn prop_covers_never_panics(a in ".*", b in ".*") {
            covers(&a, &b);
        }
    }

    /// The probe's output: the files that exist, each after a `==== <path>`
    /// line, then `==== end`. Only `permissions.deny` strings at the top
    /// level count; noise before the first marker is ssh rc-file output.
    #[test]
    fn parse_denies_reads_the_deny_lists_of_the_files_printed() {
        let out = "welcome to pi\n\
==== /home/user/.gemini/antigravity-cli/settings.json\n\
{\"agentMode\": \"accept-edits\", \"permissions\": {\"allow\": [\"command(git)\"], \"deny\": [\"command(sudo)\", 7, \"write_file(*)\"], \"ask\": [\"command(git push)\"]}}\n\
==== /home/user/.gemini/config/config.json\n\
{\"plugins\": {}}\n\
==== end\n";
        assert_eq!(
            parse_denies(out).unwrap(),
            vec!["command(sudo)", "write_file(*)"]
        );
        assert_eq!(parse_denies("==== end\n").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_denies(
                "==== /home/user/.gemini/config/config.json\n{\"permissions\": {\"deny\": \"command(sudo)\"}}\n==== end\n"
            )
            .unwrap(),
            Vec::<String>::new()
        );
        let broken = parse_denies(
            "==== /home/user/.gemini/config/config.json\n{\"permissions\": \n==== end\n",
        )
        .unwrap_err();
        assert!(broken.contains("config.json"), "{broken}");
        let cut = parse_denies("==== /home/user/.gemini/config/config.json\n{}\n").unwrap_err();
        assert!(cut.contains("incomplete"), "{cut}");
        assert!(parse_denies("").unwrap_err().contains("incomplete"));
    }

    /// The command prints exactly that, from a `HOME` with one, both or no
    /// files, and cuts a fat file, which then is not JSON: refused, never
    /// a transport error.
    #[test]
    fn the_rules_command_prints_the_files_that_exist() {
        let home = tempfile::tempdir().unwrap();
        let run = || {
            let out = std::process::Command::new("sh")
                .args(["-c", RULES_COMMAND])
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(run(), "==== end\n");
        let cli = home.path().join(".gemini/antigravity-cli");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(
            cli.join("settings.json"),
            r#"{"permissions":{"deny":["command(sudo)"]}}"#,
        )
        .unwrap();
        let out = run();
        assert_eq!(parse_denies(&out).unwrap(), vec!["command(sudo)"]);
        assert!(
            out.starts_with(&format!(
                "==== {}/.gemini/antigravity-cli/settings.json\n",
                home.path().display()
            )),
            "{out}"
        );
        let shared = home.path().join(".gemini/config");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            shared.join("config.json"),
            "{\"permissions\":{\"deny\":[\"write_file(*)\"]}}",
        )
        .unwrap();
        assert_eq!(
            parse_denies(&run()).unwrap(),
            vec!["command(sudo)", "write_file(*)"]
        );
        assert_eq!(
            parse_denies(&fake_output(&["command(sudo)"])).unwrap(),
            vec!["command(sudo)"]
        );
        let fat = format!(
            "{{\"pad\": \"{}\", \"permissions\": {{\"deny\": [\"command(sudo)\"]}}}}",
            "x".repeat(40_000)
        );
        std::fs::write(cli.join("settings.json"), fat).unwrap();
        let out = run();
        assert!(out.len() < 64 * 1024, "{}", out.len());
        assert!(parse_denies(&out).unwrap_err().contains("settings.json"));
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
