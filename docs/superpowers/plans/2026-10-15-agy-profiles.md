# Permission profiles for agy: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a pastor task under a permission profile whose agent is agy never stops at a prompt and never does what the profile denies, or is refused with a code that says why.

**Architecture:** agy takes no policy at launch, so pastor adds the two flags that stop it asking (`--dangerously-skip-permissions --mode accept-edits`), translates the task's deny list into agy's rule syntax, and before launch reads agy's two settings files off the machine to check every required deny is there. A deny with no agy rule is refused at queue time; an unpinned task skips a machine whose own profile does not cover its denies. A new module `src/config/agy.rs` holds the translation, the covering rules and the probe command, on the pattern of `src/config/opencode.rs`.

**Tech Stack:** Rust 2024, serde_json, proptest, tokio tests, the fake herdr behind the `fake-herdr` feature. `make check` is the gate (fmt, clippy with warnings as errors, the full suite, the change file check).

**Spec:** `docs/superpowers/specs/2026-10-15-agy-profiles-design.md`

## Global Constraints

- The two flags are exactly `--dangerously-skip-permissions` and `--mode accept-edits`, added after the task's `agent_args`, only when `spec.profile()` is `Some` and the agent's kind is `agy` and its definition has no `own_permissions`.
- `own_permissions = true` keeps today's behaviour on every kind: no flag, no mode, no check (`launch_args` returns the args alone).
- Codes, as `&'static str`: `agy_deny_unsupported`, `agy_permissions_missing`, `agy_profile_mismatch`. Every refusal names what to do.
- The probe reads only `$HOME/.gemini/antigravity-cli/settings.json` and `$HOME/.gemini/config/config.json`. Project files under `~/.gemini/config/projects/` are never read.
- Covering: `regex:` rules, `ask` and `allow` entries cover nothing. Paths cover on component boundaries (`std::path::Path::starts_with`). A relative settings path covers only an identical required path.
- A `command` machine (no probe), an unparsable file and an oversized probe all refuse the task; nothing goes ahead unverified.
- Claude, Codex, opencode and custom kinds are unchanged; without a profile nothing changes for agy either.
- Nothing from the fleet in a commit: no hostnames, user names or home paths. Examples use `pi-1`, `user@pi-1`, `/home/user`.
- Commit messages: conventional prefix, plain subject, a body with the why, no trailers. Commits are signed by the repo's git config; never pass `--no-gpg-sign`. If signing fails, stop and show the error.
- Docs that name a command or flag are checked by a unit test under `make check` (fences tagged `text`, `toml`, `json` are skipped; every `toml` fence must parse).

## Review Focus

1. A deny pattern with an empty command, `Bash(:*)` or `Bash()`: agy treats an entry with no words as matching nothing, so pastor must refuse it (`agy_deny_unsupported`), never require `command()`. Test in Task 1.
2. A settings file with `"permissions": {"deny": "command(sudo)"}` (a string, not an array) or a non-string entry in the array: must count as no rules, not panic, and not count as covering. Test in Task 3.
3. ssh rc-file noise before the first `==== ` line, and a probe cut off before `==== end`: the first is ignored, the second refuses. Test in Task 3.
4. A deny path with a trailing slash in settings, `write_file(/srv/app/)`, against a required `write_file(/srv/app/src)`: must cover; `write_file(/srv/app)` against `write_file(/srv/application)`: must not. Test in Task 2.
5. A task with a profile on an agy agent that also has `allow_flag`/`deny_flag` set in `[agents.agy]`: the flags must not be used under a profile (agy's lists are not passed), and without a profile they still are. Test in Task 4.

---

### Task 1: `config::agy`: constants and the deny translation

**Files:**
- Create: `src/config/agy.rs`
- Modify: `src/config/mod.rs:1-4` (add `pub mod agy;`)

**Interfaces:**
- Produces: `pub const KIND: &str = "agy"`, `pub const NO_ASK: [&str; 3]`, `pub const AGY_DENY_UNSUPPORTED`, `AGY_PERMISSIONS_MISSING`, `AGY_PROFILE_MISMATCH: &str`, `pub const GLOBAL_SETTINGS: &str`, `pub const SHARED_CONFIG: &str`, `pub fn required_rule(pattern: &str) -> Option<String>`, `pub fn is_permission_arg(arg: &str) -> bool`.

- [ ] **Step 1: Create the module with its header and constants, and the failing tests**

```rust
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
        let p = p.strip_suffix("/**").or_else(|| p.strip_suffix("/*")).unwrap_or(p);
        if p.is_empty() { "*".to_string() } else { p.to_string() }
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
                Some((server, t)) if !server.is_empty() && !t.is_empty() => format!("mcp({server}/{t})"),
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
            "Glob", "Grep", "WebSearch", "Task", "Foo", "Bash(:*)", "Bash()", "Bash( )",
            "WebFetch(example.com)", "mcp__", "mcp____x", "mcp__a__b(c)",
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
```

- [ ] **Step 2: Register the module and run the tests to see them fail**

Add `pub mod agy;` as the first line of `src/config/mod.rs` (before `pub mod flock;`).

Run: `cargo test --features fake-herdr config::agy`
Expected: the module did not exist before this step, so the run proves the tests compile and run; they should PASS, since the code and tests land together here. If one fails, fix the code, not the table.

- [ ] **Step 3: Run to green and check clippy**

Run: `cargo test --features fake-herdr config::agy && cargo clippy --all-targets --features fake-herdr -- -D warnings`
Expected: PASS, no warnings. Fix the `path` closure's `if` formatting if `cargo fmt --check` complains (run `cargo fmt`).

- [ ] **Step 4: Commit**

```bash
git add src/config/agy.rs src/config/mod.rs
git commit -m "feat(agy): translate a deny list into agy's rule syntax

agy takes no policy at launch; its rules live in its settings on the
machine. The first piece of a profile for it is knowing which rule each
Claude pattern in a deny list would need there, and which patterns agy has
no rule for."
```

---

### Task 2: `config::agy`: covering rules

**Files:**
- Modify: `src/config/agy.rs`

**Interfaces:**
- Produces: `pub fn covers(deny: &str, required: &str) -> bool`, `pub fn missing(required: &[String], denies: &[String]) -> Vec<String>`, `pub fn not_denied_by(task_deny: &[String], own_deny: &[String]) -> Vec<String>`.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
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
        let review = crate::config::profile::Profiles::default().resolve("review").unwrap();
        let required: Vec<String> = review.deny.iter().filter_map(|p| required_rule(p)).collect();
        assert_eq!(
            required,
            strings(&["write_file(*)", "write_file(*)", "write_file(*)", "command(git push)",
                      "command(rm -rf)", "command(sudo)", "command(git push --force)"])
        );
        let full = strings(&["write_file(*)", "command(git push)", "command(rm -rf)", "command(sudo)"]);
        assert!(missing(&required, &full).is_empty());
        let short = strings(&["write_file(*)", "command(rm -rf)", "command(sudo)", "command(git push --force)"]);
        assert_eq!(missing(&required, &short), strings(&["command(git push)"]));
        assert_eq!(
            missing(&required, &[]),
            strings(&["write_file(*)", "command(git push)", "command(rm -rf)", "command(sudo)",
                      "command(git push --force)"])
        );
    }

    /// A task's deny patterns not in the machine's own profile's, as
    /// written (Claude patterns, string equality).
    #[test]
    fn not_denied_by_compares_claude_patterns() {
        let strings = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let own = strings(&["Edit", "Bash(sudo:*)"]);
        assert_eq!(
            not_denied_by(&strings(&["Edit", "Bash(git push:*)", "Bash(sudo:*)"]), &own),
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
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --features fake-herdr config::agy`
Expected: compile error, `covers` not found.

- [ ] **Step 3: Implement**

Add above the `tests` module:

```rust
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
        "mcp" => rt == dt || dt.strip_suffix("/*").is_some_and(|s| rt.strip_prefix(s).is_some_and(|t| t.starts_with('/'))),
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
```

- [ ] **Step 4: Run to green, fmt, clippy**

Run: `cargo fmt && cargo test --features fake-herdr config::agy && cargo clippy --all-targets --features fake-herdr -- -D warnings`
Expected: PASS. If the `read_url` arm fails on `api.example.com` vs `example.com`: `"api.example.com".strip_suffix("example.com")` is `Some("api.")`, which ends with `.`, so it covers; `"example.com".strip_suffix("ample.com")` is `Some("ex")`, which does not. Keep the arm.

- [ ] **Step 5: Commit**

```bash
git add src/config/agy.rs
git commit -m "feat(agy): say when a settings deny covers a required rule

What a profile needs on an agy machine is a deny for each pattern, and
agy's own matching decides what counts: a command prefix by words, a path
by directory, a host by domain. A regex covers nothing pastor can prove."
```

---

### Task 3: `config::agy`: the probe command and its parser; `Connector::agy_rules`

**Files:**
- Modify: `src/config/agy.rs`
- Modify: `src/herdr/transport.rs` (trait default near line 416, `Endpoint` impl near line 468, a new async fn near line 612, a test near line 1321)
- Modify: `src/herdr/fake.rs` (state field near line 101, default near line 238, setter near line 254, trait impl near line 1381)

**Interfaces:**
- Produces: `pub const RULES_COMMAND: &str`, `pub fn parse_denies(output: &str) -> Result<Vec<String>, String>`, `pub fn fake_output(denies: &[&str]) -> String` (behind `#[cfg(any(test, feature = "fake-herdr"))]`), `Connector::agy_rules(&self) -> VersionFuture<'_>` (the probe's raw stdout, `None` when the machine takes no shell command), `FakeHerdr::set_agy_rules(&self, output: Option<String>)`.

- [ ] **Step 1: Write the failing parser tests**

Append to the `tests` module of `src/config/agy.rs`:

```rust
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
        assert_eq!(parse_denies(out).unwrap(), vec!["command(sudo)", "write_file(*)"]);
        assert_eq!(parse_denies("==== end\n").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_denies("==== /home/user/.gemini/config/config.json\n{\"permissions\": {\"deny\": \"command(sudo)\"}}\n==== end\n").unwrap(),
            Vec::<String>::new()
        );
        let broken = parse_denies("==== /home/user/.gemini/config/config.json\n{\"permissions\": \n==== end\n").unwrap_err();
        assert!(broken.contains("config.json"), "{broken}");
        let cut = parse_denies("==== /home/user/.gemini/config/config.json\n{}\n").unwrap_err();
        assert!(cut.contains("incomplete"), "{cut}");
        assert!(parse_denies("").unwrap_err().contains("incomplete"));
    }

    /// The command prints exactly that, from a `HOME` with one, both or no
    /// files.
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
        std::fs::write(cli.join("settings.json"), r#"{"permissions":{"deny":["command(sudo)"]}}"#).unwrap();
        let out = run();
        assert_eq!(parse_denies(&out).unwrap(), vec!["command(sudo)"]);
        assert!(out.starts_with(&format!("==== {}/.gemini/antigravity-cli/settings.json\n", home.path().display())), "{out}");
        let shared = home.path().join(".gemini/config");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("config.json"), "{\"permissions\":{\"deny\":[\"write_file(*)\"]}}").unwrap();
        assert_eq!(parse_denies(&run()).unwrap(), vec!["command(sudo)", "write_file(*)"]);
        assert_eq!(parse_denies(&fake_output(&["command(sudo)"])).unwrap(), vec!["command(sudo)"]);
        // A fat file is cut, and a cut file is not JSON: refused, never a
        // transport error.
        let fat = format!("{{\"pad\": \"{}\", \"permissions\": {{\"deny\": [\"command(sudo)\"]}}}}", "x".repeat(40_000));
        std::fs::write(cli.join("settings.json"), fat).unwrap();
        let out = run();
        assert!(out.len() < 64 * 1024, "{}", out.len());
        assert!(parse_denies(&out).unwrap_err().contains("settings.json"));
    }
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --features fake-herdr config::agy`
Expected: compile error, `parse_denies` and `RULES_COMMAND` not found.

- [ ] **Step 3: Implement the command, the parser and the fake output**

Add above the `tests` module:

```rust
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
        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("{path} is not JSON ({e})"))?;
        if let Some(list) = json.get("permissions").and_then(|p| p.get("deny")).and_then(|d| d.as_array()) {
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
```

- [ ] **Step 4: Run to green**

Run: `cargo fmt && cargo test --features fake-herdr config::agy`
Expected: PASS.

- [ ] **Step 5: Write the failing transport test**

In `src/herdr/transport.rs` tests, after `opencode_permission_rules_per_endpoint`, add the same child-process shape (HOME is process-wide, so the local half runs in a child of the test binary):

```rust
    /// `agy_rules` prints agy's settings from the machine's `HOME`, and a
    /// command machine answers nothing. The local half runs in a child with
    /// its own `HOME`, as the opencode test does.
    #[tokio::test]
    async fn agy_rules_per_endpoint() {
        const CHILD: &str = "PASTOR_TEST_AGY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let local = Endpoint::Local {
                session: "s".into(),
            };
            let out = local.agy_rules().await.unwrap().unwrap();
            assert_eq!(crate::config::agy::parse_denies(&out).unwrap(), vec!["command(sudo)"]);
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let cli = home.path().join(".gemini/antigravity-cli");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(cli.join("settings.json"), r#"{"permissions":{"deny":["command(sudo)"]}}"#).unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "herdr::transport::tests::agy_rules_per_endpoint", "--test-threads=1"])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
        assert!(stdout.contains("1 passed"), "{stdout}");
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.agy_rules().await.unwrap(), None);
    }
```

- [ ] **Step 6: Run to see it fail**

Run: `cargo test --features fake-herdr agy_rules_per_endpoint`
Expected: compile error, no method `agy_rules`.

- [ ] **Step 7: Implement the trait method, the endpoint half and the fake**

In the `Connector` trait, after `opencode_permission_rules`:

```rust
    /// What agy's settings on the machine say, as `config::agy::RULES_COMMAND`
    /// prints it, for `config::agy::parse_denies`. `None` when the machine
    /// takes no shell command.
    fn agy_rules(&self) -> VersionFuture<'_> {
        Box::pin(async { Ok(None) })
    }
```

In `impl Connector for Endpoint`, after `opencode_permission_rules`:

```rust
    fn agy_rules(&self) -> VersionFuture<'_> {
        Box::pin(agy_rules(self))
    }
```

After the `opencode_permission_rules` async fn:

```rust
async fn agy_rules(ep: &Endpoint) -> Result<Option<String>, ConnectError> {
    let command = crate::config::agy::RULES_COMMAND.to_string();
    match probe(ep, command).await? {
        Some(p) => {
            reached(&p.target, &p.out)?;
            Ok(Some(String::from_utf8_lossy(&p.out.stdout).into_owned()))
        }
        None => Ok(None),
    }
}
```

In `src/herdr/fake.rs`: a state field `agy_rules: Option<String>` with a doc comment `/// What `Connector::agy_rules` reports.`; in `FakeHerdr::new` set `agy_rules: Some(crate::config::agy::fake_output(&["write_file(*)", "command(git push)", "command(rm -rf)", "command(sudo)"]))` so a profiled agy task goes ahead by default, as opencode's `Some(false)` does; a setter:

```rust
    /// What agy's settings on the machine say (`config::agy::fake_output`);
    /// `None` for a machine that takes no shell command.
    pub fn set_agy_rules(&self, output: Option<String>) {
        self.state.lock().unwrap().agy_rules = output;
    }
```

and the trait impl next to `opencode_permission_rules`:

```rust
    fn agy_rules(&self) -> super::transport::VersionFuture<'_> {
        let out = self.state.lock().unwrap().agy_rules.clone();
        Box::pin(async move { Ok(out) })
    }
```

If `State` derives `Default` and the field is `Option<String>`, nothing else is needed.

- [ ] **Step 8: Run to green, clippy**

Run: `cargo fmt && cargo test --features fake-herdr agy && cargo clippy --all-targets --features fake-herdr -- -D warnings`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/config/agy.rs src/herdr/transport.rs src/herdr/fake.rs
git commit -m "feat(agy): read agy's deny rules off a machine

One probe prints agy's global settings and shared config, the two files
whose rules apply to every session on the machine, and the head reads
their deny lists. Project files are left out: agy applies only the one for
its active workspace, which pastor cannot tell."
```

---

### Task 4: `launch_args`, `lists_unapplied`, `agy_profile`

**Files:**
- Modify: `src/config/mod.rs:1388-1430` (`lists_unapplied`, `opencode_profile`, `own_permissions`), `src/config/mod.rs:1446-1470` (`launch_args`), tests near line 3748 (`an_agent_with_its_own_permissions_starts_under_a_profile_without_flags`)

**Interfaces:**
- Consumes: `agy::KIND`, `agy::NO_ASK`, `agy::is_permission_arg`, `agy::required_rule`, `agy::AGY_DENY_UNSUPPORTED`.
- Produces: `Agents::agy_profile(&self, spec: &DispatchSpec) -> bool`.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `src/config/mod.rs`, next to the own_permissions test, add:

```rust
    /// An agy agent under a profile starts with the two flags that stop it
    /// asking and no tool flags, even with `allow_flag`/`deny_flag` set;
    /// `task describe` says the lists are not passed. Args that pick its
    /// mode are refused, as is a deny pattern agy has no rule for. With
    /// `own_permissions` nothing is added, and without a profile the lists
    /// still need flags.
    #[test]
    fn an_agy_agent_under_a_profile_starts_with_the_skip_flag_and_accept_edits() {
        let mut spec = spec_with("agy", &["Read"], &["Edit", "Bash(sudo:*)"]);
        spec.agent_source = Some(Box::new(crate::task::AgentSource {
            agent: "defaults".into(),
            profile: Some("review".into()),
            profile_from: Some("defaults".into()),
            ..Default::default()
        }));
        let agents = Agents::default();
        assert!(agents.agy_profile(&spec));
        let launch = agents.launch(&spec).unwrap();
        assert_eq!(launch.kind, "agy");
        assert_eq!(
            launch.args,
            vec!["--model", "m", "--dangerously-skip-permissions", "--mode", "accept-edits"]
        );
        assert!(launch.env.is_empty());
        let why = agents.lists_unapplied(&spec).unwrap();
        assert!(why.contains("deny was checked"), "{why}");
        assert!(why.contains("allow is not passed"), "{why}");

        // Flags in its definition are not used under a profile.
        let flagged: Agents =
            toml::from_str("[agy]\nallow_flag = \"--allow\"\ndeny_flag = \"--deny\"\n").unwrap();
        assert_eq!(flagged.launch(&spec).unwrap().args, launch.args);

        for arg in ["--mode", "--mode=plan", "--dangerously-skip-permissions"] {
            let mut s = spec.clone();
            s.agent_args.push(arg.into());
            let err = agents.launch(&s).unwrap_err();
            assert_eq!(err.code, PROFILE_ARGS_CONFLICT, "{arg}");
            assert!(err.message.contains(arg), "{err}");
        }
        let mut s = spec.clone();
        s.agent_args.push("--sandbox".into());
        assert!(agents.launch(&s).is_ok());

        let mut s = spec.clone();
        s.deny.push("WebSearch".into());
        let err = agents.launch(&s).unwrap_err();
        assert_eq!(err.code, agy::AGY_DENY_UNSUPPORTED);
        assert!(err.message.contains("WebSearch"), "{err}");

        let own: Agents = toml::from_str("[agy]\nown_permissions = true\n").unwrap();
        assert!(!own.agy_profile(&spec));
        assert_eq!(own.launch(&spec).unwrap().args, vec!["--model", "m"]);

        // A definition of kind agy under another name.
        let mine: Agents = toml::from_str("[agy-review]\nkind = \"agy\"\n").unwrap();
        spec.agent = "agy-review".into();
        assert!(mine.agy_profile(&spec));
        assert_eq!(mine.launch(&spec).unwrap().args, launch.args);

        // Without a profile: no flags added, and the lists need flags.
        spec.agent_source = None;
        assert!(!mine.agy_profile(&spec));
        assert_eq!(mine.launch(&spec).unwrap_err().code, "agent_tools_unsupported");
        assert_eq!(mine.lists_unapplied(&spec), None);
        assert_eq!(
            flagged.launch(&spec).unwrap().args,
            vec!["--model", "m", "--allow", "Read", "--deny", "Edit", "--deny", "Bash(sudo:*)"]
        );
    }
```

Then change the existing `an_agent_with_its_own_permissions_starts_under_a_profile_without_flags`: delete its first block (the `unwrap_err()` on `Agents::default().launch(&spec)` asserting `agent_tools_unsupported` and the `own_permissions` message, and the `lists_unapplied` `None` assertion), since an agy agent without the field now starts; keep the rest.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --features fake-herdr an_agy_agent_under_a_profile`
Expected: compile error, no method `agy_profile`.

- [ ] **Step 3: Implement**

In `impl Agents`, next to `opencode_profile`:

```rust
    /// Does `spec` run an agy agent under a permission profile? It then
    /// starts with `agy::NO_ASK` and dispatch checks its settings on the
    /// machine (`config::agy`).
    pub fn agy_profile(&self, spec: &crate::task::DispatchSpec) -> bool {
        spec.profile().is_some()
            && self.kind(&spec.agent) == agy::KIND
            && !self.own_permissions(&spec.agent)
    }
```

In `lists_unapplied`, after the `own_permissions` branch and before the codex one:

```rust
        if self.agy_profile(spec) {
            return (!spec.allow.is_empty() || !spec.deny.is_empty()).then(|| {
                "agy takes no lists: the deny was checked against its settings on the machine, the allow is not passed".to_string()
            });
        }
```

In `launch_args`, after the `own_permissions` early return and before the claude branch:

```rust
        if let Some(profile) = spec.profile()
            && self.agy_profile(spec)
        {
            if let Some(arg) = args.iter().find(|a| agy::is_permission_arg(a)) {
                return Err(AgentRefusal {
                    code: PROFILE_ARGS_CONFLICT,
                    message: format!(
                        "task runs under profile {profile}, and agent {}'s args set {arg}; \
                         drop it from agent_args or the model's args, or run without a profile",
                        spec.agent
                    ),
                });
            }
            if let Some(p) = spec.deny.iter().find(|p| agy::required_rule(p).is_none()) {
                return Err(AgentRefusal {
                    code: agy::AGY_DENY_UNSUPPORTED,
                    message: format!(
                        "task runs under profile {profile} and denies {p}, which agy has no rule for; \
                         drop it from the profile or the task's deny list, or run without a profile"
                    ),
                });
            }
            args.extend(agy::NO_ASK.map(str::to_string));
            return Ok(args);
        }
```

Update the doc comment of `launch_args` to add: "an agy agent under a profile gets `--dangerously-skip-permissions --mode accept-edits` and no tool flags (`config::agy`), and is refused a deny pattern agy has no rule for (`agy_deny_unsupported`)". Update the doc comment of `AgentRefusal.code` (near line 1594) to list the three new codes. Update `AgentDef::own_permissions`'s comment to drop "(agy's allow list)" and say "for an agent whose kind pastor cannot drive, or an agy owner who manages the file by hand".

- [ ] **Step 4: Run to green, the whole config suite, clippy**

Run: `cargo fmt && cargo test --features fake-herdr config:: && cargo clippy --all-targets --features fake-herdr -- -D warnings`
Expected: PASS. The old own_permissions test passes with its first block removed.

- [ ] **Step 5: Commit**

```bash
git add src/config/mod.rs
git commit -m "feat(agy): start a profiled agy agent so it never asks

Under a profile an agy agent gets the flag that approves every ask and the
mode that skips the diff review, and no tool flags, since it has none. A
deny pattern agy has no rule for is refused when the task is queued, not
dropped: a deny dropped without a word is worse than not starting."
```

---

### Task 5: the machine's own profile must cover a profiled agy task

**Files:**
- Modify: `src/daemon.rs:788-800` (`settle`, after `profiles.apply`), `src/daemon.rs:930-935` (`per_machine` in `settle_agent`), tests near line 8870

**Interfaces:**
- Consumes: `agy::not_denied_by`, `agy::AGY_PROFILE_MISMATCH`, `Agents::agy_profile`, `Profiles::resolve`.

- [ ] **Step 1: Write the failing test**

In the daemon tests after `unrestricted_runs_only_on_a_machine_that_is_unrestricted`:

```rust
    /// agy's rules are per machine, so a profiled agy task runs only on a
    /// machine whose own profile denies everything the task's does: an
    /// unpinned `review` task skips a `develop` machine for a `review` one,
    /// pinned to the `develop` machine it is refused, and a machine with no
    /// own profile is tried.
    #[tokio::test]
    async fn a_profiled_agy_task_needs_a_machine_whose_own_profile_covers_it() {
        let dev = MachineConfig {
            profile: Some("develop".into()),
            ..machine("dev", 2)
        };
        let rev = MachineConfig {
            profile: Some("review".into()),
            ..machine("rev", 1)
        };
        let (d, _tmp) = profiles_daemon(
            vec![dev, rev, machine("pi", 2)],
            &[("dev", 2, FakeHerdr::new()), ("rev", 1, FakeHerdr::new()), ("pi", 2, FakeHerdr::new())],
        )
        .await;
        let run = |profile: Option<&str>, machine: Option<&str>| {
            let IpcRequest::Run { now, preempt, summary, prompt, spec, flock, priority, role, description, agent: Some(choice) } =
                run_profile(profile, machine)
            else {
                unreachable!()
            };
            IpcRequest::Run {
                now, preempt, summary, prompt, spec, flock, priority, role, description,
                agent: Some(AgentChoice { agent: Some("agy".into()), ..choice }),
            }
        };
        let resp = d.handle(run(Some("review"), Some("dev"))).await;
        let IpcResponse::Error { code, message } = resp else { panic!("{resp:?}") };
        assert_eq!(code, crate::config::agy::AGY_PROFILE_MISMATCH);
        assert!(message.contains("Bash(git push:*)"), "{message}");
        assert!(message.contains("develop"), "{message}");

        let IpcResponse::Task(t) = d.handle(run(Some("review"), None)).await else { panic!() };
        assert_eq!(t.machine.as_deref(), Some("rev"));
        // rev is full and dev does not cover review; pi has no own profile,
        // so it is tried and the dispatch check decides.
        let IpcResponse::Task(t) = d.handle(run(Some("review"), None)).await else { panic!() };
        assert_eq!(t.machine.as_deref(), Some("pi"));
        // develop on the develop machine is fine.
        let IpcResponse::Task(t) = d.handle(run(Some("develop"), Some("dev"))).await else { panic!() };
        assert_eq!(t.machine.as_deref(), Some("dev"));
    }
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test --features fake-herdr a_profiled_agy_task_needs_a_machine_whose_own_profile_covers_it`
Expected: FAIL: the pinned run returns a Task, not an error.

- [ ] **Step 3: Implement**

In `Fleet::settle` (daemon.rs), after the `self.profiles...apply(&pick, own.as_deref(), spec)?;` line:

```rust
        // agy's rules are per machine: a task's profile has to be one the
        // machine's own profile already denies (`config::agy`).
        if agents.agy_profile(spec)
            && let Some(own) = own.as_deref()
        {
            let own_deny = self.profiles.read().recover().resolve(own)?.deny;
            let uncovered = crate::config::agy::not_denied_by(&spec.deny, &own_deny);
            if !uncovered.is_empty() {
                return Err(AgentRefusal {
                    code: crate::config::agy::AGY_PROFILE_MISMATCH,
                    message: format!(
                        "profile {} denies {} and the own profile of machine {} ({own}) does not; \
                         agy's rules are per machine, so run the task under that machine's profile \
                         or on a machine whose own profile is {}",
                        spec.profile().unwrap_or_default(),
                        uncovered.join(", "),
                        machine.unwrap_or("-"),
                        spec.profile().unwrap_or_default(),
                    ),
                });
            }
        }
```

In `settle_agent`, extend `per_machine`:

```rust
        let per_machine = |e: &AgentRefusal| {
            pinned.is_none()
                && ((e.code == MODEL_KIND_MISMATCH && ask.agent.is_none())
                    || e.code == AGENT_KIND_MISSING
                    || e.code == PROFILE_NOT_ALLOWED
                    || e.code == crate::config::agy::AGY_PROFILE_MISMATCH)
        };
```

and its comment: add "and so is which profile an agy machine runs".

- [ ] **Step 4: Run to green and the daemon suite**

Run: `cargo fmt && cargo test --features fake-herdr daemon:: && cargo clippy --all-targets --features fake-herdr -- -D warnings`
Expected: PASS. If the second unpinned run lands on `dev` instead of `pi`, check machine order in `pick_machine`: the test does not depend on order between `dev` and `pi` being tried, only that `dev` is skipped; if it fails, read the `waiting` reason and the `per_machine` closure.

- [ ] **Step 5: Commit**

```bash
git add src/daemon.rs
git commit -m "feat(agy): place a profiled agy task where the machine's profile covers it

agy's rules are one file per machine, so an agy machine runs one profile.
A task that asks for a narrower one than the machine's own cannot be
narrowed there; unpinned it skips that machine, pinned it is refused."
```

---

### Task 6: the dispatch check

**Files:**
- Modify: `src/dispatch.rs:405-425` (after the opencode check), tests near line 2270
- Modify: `src/daemon.rs` tests near line 8724 (`an_agy_task_under_a_profile_needs_its_own_permissions`)

**Interfaces:**
- Consumes: `Connector::agy_rules`, `agy::parse_denies`, `agy::required_rule`, `agy::missing`, `agy::AGY_PERMISSIONS_MISSING`, `agy::GLOBAL_SETTINGS`, `Agents::agy_profile`.

- [ ] **Step 1: Write the failing dispatch tests**

After `a_profiled_opencode_task_refuses_a_machine_with_its_own_rules` in `src/dispatch.rs` tests:

```rust
    fn agy_under(profile: Option<&str>) -> DispatchSpec {
        DispatchSpec {
            agent: "agy".into(),
            ..opencode_under(profile)
        }
    }

    /// A profiled agy task starts with the flags that stop it asking when
    /// the machine's agy settings deny what it denies; with a rule missing,
    /// with settings pastor cannot read, and on a machine that takes no
    /// shell command, it fails before anything is made there. Without a
    /// profile nothing is checked.
    #[tokio::test]
    async fn a_profiled_agy_task_needs_its_denies_in_the_machines_agy_settings() {
        let fake = FakeHerdr::new();
        let mut t = task(agy_under(Some("review")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY).await.unwrap();
        let start = fake.requests();
        let start = start.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "agy");
        assert_eq!(
            start.params["args"],
            serde_json::json!(["--dangerously-skip-permissions", "--mode", "accept-edits"])
        );

        let refused = |rules: Option<String>, says: &'static str| async move {
            let fake = FakeHerdr::new();
            fake.set_agy_rules(rules);
            let mut t = task(agy_under(Some("review")));
            let err = dispatch(&fake, &mut t, &Agents::default(), None, READY).await.unwrap_err();
            let text = err.to_string();
            assert!(text.contains("agy_permissions_missing"), "{text}");
            assert!(text.contains(says), "{text}");
            assert!(text.contains("pi-1"), "{text}");
            assert!(!err.is_transport());
            assert_eq!(t.state, TaskState::Failed);
            assert!(!fake.requests().iter().any(|r| r.method == "agent.start"));
        };
        refused(Some(crate::config::agy::fake_output(&["command(sudo)"])), "write_file(*)").await;
        refused(Some("==== /home/fake/.gemini/config/config.json\n{\n==== end\n".into()), "config.json").await;
        refused(None, "cannot be read").await;

        let fake = FakeHerdr::new();
        fake.set_agy_rules(None);
        let mut t = task(agy_under(None));
        dispatch(&fake, &mut t, &Agents::default(), None, READY).await.unwrap();
    }
```

Note `agy_under(None)` carries `deny: ["Edit"]` with no profile and the agent has no `deny_flag`; `launch` refuses that with `agent_tools_unsupported` before any check. So for the last case use a spec with empty lists: replace the last four lines with

```rust
        let fake = FakeHerdr::new();
        fake.set_agy_rules(None);
        let mut t = task(DispatchSpec { allow: vec![], deny: vec![], ..agy_under(None) });
        dispatch(&fake, &mut t, &Agents::default(), None, READY).await.unwrap();
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test --features fake-herdr a_profiled_agy_task_needs_its_denies`
Expected: FAIL: the first dispatch's args are `[]`? No: Task 4 already adds the flags. The `refused` cases fail: dispatch succeeds where it must refuse.

- [ ] **Step 3: Implement**

In `src/dispatch.rs`, right after the opencode conflict `if` block:

```rust
    // agy's rules are in its settings on the machine, and under the flags
    // pastor adds everything they do not deny runs: so what the task denies
    // has to be denied there, or the task does not start. A machine that
    // cannot be asked refuses too, unlike opencode's check: going ahead
    // would mean the skip flag with nothing verified behind it.
    if agents.agy_profile(&spec) && !spec.deny.is_empty() {
        let machine = task.machine.as_deref().unwrap_or("this machine");
        let profile = spec.profile().unwrap_or_default();
        let required: Vec<String> = spec
            .deny
            .iter()
            .filter_map(|p| crate::config::agy::required_rule(p))
            .collect();
        let why = match conn.agy_rules().await.map_err(CallError::from)? {
            None => Some(format!(
                "agy's permission rules on {machine} cannot be read (the machine takes no shell command), \
                 so pastor cannot check that profile {profile} holds there"
            )),
            Some(out) => match crate::config::agy::parse_denies(&out) {
                Err(e) => Some(format!("agy's permission rules on {machine} could not be read: {e}")),
                Ok(denies) => {
                    let missing = crate::config::agy::missing(&required, &denies);
                    (!missing.is_empty()).then(|| {
                        format!(
                            "agy on {machine} does not deny what profile {profile} denies; add to \
                             \"permissions\".\"deny\" in {}:\n{}",
                            crate::config::agy::GLOBAL_SETTINGS,
                            missing.join("\n")
                        )
                    })
                }
            },
        };
        if let Some(why) = why {
            return Err(DispatchError::Task(format!(
                "{}: {why}",
                crate::config::agy::AGY_PERMISSIONS_MISSING
            )));
        }
    }
```

- [ ] **Step 4: Run to green**

Run: `cargo fmt && cargo test --features fake-herdr dispatch:: && cargo clippy --all-targets --features fake-herdr -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Rewrite the daemon end-to-end test**

Replace `an_agy_task_under_a_profile_needs_its_own_permissions` in `src/daemon.rs` tests with:

```rust
    /// End to end: a profiled agy task starts with the flags that stop it
    /// asking and `task describe` says its lists are not passed; on a
    /// machine whose agy settings lack a rule it fails before `agent.start`,
    /// naming the rule; `unrestricted`, which denies nothing, runs no probe;
    /// with `own_permissions` nothing is added or checked.
    #[tokio::test]
    async fn an_agy_task_under_a_profile_starts_without_asking_or_fails_naming_the_rule() {
        let run_agy = |profile: Option<&str>| {
            let IpcRequest::Run { now, preempt, summary, prompt, spec, flock, priority, role, description, agent: Some(choice) } =
                run_profile(profile, None)
            else {
                unreachable!()
            };
            IpcRequest::Run {
                now, preempt, summary, prompt, spec, flock, priority, role, description,
                agent: Some(AgentChoice { agent: Some("agy".into()), ..choice }),
            }
        };
        let fake = FakeHerdr::new();
        let (d, _tmp) = profiles_daemon(vec![machine("pi", 4)], &[("pi", 4, fake.clone())]).await;
        let IpcResponse::Task(t) = d.handle(run_agy(Some("develop"))).await else { panic!() };
        assert_eq!(t.profile(), Some("develop"));
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "agy");
        let args: Vec<&str> = start.params["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
        assert_eq!(&args[args.len() - 3..], ["--dangerously-skip-permissions", "--mode", "accept-edits"]);
        for d in &t.spec.deny {
            assert!(!args.contains(&d.as_str()), "{args:?}");
        }
        let text = crate::cli::task_detail(&t);
        assert!(text.contains("(agy takes no lists:"), "{text}");

        fake.set_agy_rules(Some(crate::config::agy::fake_output(&["command(sudo)"])));
        let before = fake.requests().iter().filter(|r| r.method == "agent.start").count();
        let IpcResponse::Task(t) = d.handle(run_agy(Some("develop"))).await else { panic!() };
        let t = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(t.state, TaskState::Failed);
        let err = t.error.clone().unwrap_or_default();
        assert!(err.contains("agy_permissions_missing"), "{err}");
        assert!(err.contains("command(rm -rf)"), "{err}");
        assert!(err.contains("command(git push --force)"), "{err}");
        assert!(!err.contains("command(sudo)"), "{err}");
        assert_eq!(fake.requests().iter().filter(|r| r.method == "agent.start").count(), before);

        // `unrestricted` denies nothing: no probe, no refusal.
        let mut config = profiles_config();
        config.defaults.profile = Some("unrestricted".into());
        apply_config(&d, &config).await;
        fake.set_agy_rules(None);
        let IpcResponse::Task(t) = d.handle(run_agy(Some("unrestricted"))).await else { panic!() };
        assert_eq!(d.store.get_task(t.id).unwrap().unwrap().state, TaskState::Running);

        // `own_permissions`: its args alone, nothing checked.
        let mut config = profiles_config();
        config.agents.0.insert(
            "agy".into(),
            crate::config::AgentDef { own_permissions: true, ..Default::default() },
        );
        apply_config(&d, &config).await;
        let IpcResponse::Task(t) = d.handle(run_agy(Some("develop"))).await else { panic!() };
        let reqs = fake.requests();
        let start = reqs.iter().filter(|r| r.method == "agent.start").last().unwrap();
        let args: Vec<&str> = start.params["args"].as_array().map(|a| a.iter().map(|a| a.as_str().unwrap()).collect()).unwrap_or_default();
        assert_eq!(args, t.spec.agent_args, "{args:?}");
        assert!(crate::cli::task_detail(&t).contains("own settings on the machine"));
    }
```

`Daemon::handle(IpcRequest::Run)` calls `dispatch_queued_for` before it returns, so a failed dispatch is in the store by then. The `develop` profile denies `Bash(rm -rf:*)`, `Bash(sudo:*)`, `Bash(git push --force:*)` (no `Edit`), so the missing list is `command(rm -rf)` and `command(git push --force)`.

- [ ] **Step 6: Run the daemon suite to green, then the whole gate**

Run: `cargo fmt && cargo test --features fake-herdr daemon:: && make check`
Expected: PASS. `make check` fails on the change file only once `changes/feat-agent-profiles.md` is missing or malformed: it is written in Task 8; if `changelog-check` complains now, write a one-line placeholder file with a `### Added` heading and one entry, and finish it in Task 8.

- [ ] **Step 7: Commit**

```bash
git add src/dispatch.rs src/daemon.rs changes/
git commit -m "feat(agy): check agy's settings deny what the profile denies before launch

Under the flags pastor adds, everything agy's settings do not deny runs,
so the settings have to deny what the task does. The check reads them off
the machine right before launch and refuses with the rules to add. A
machine that cannot be asked refuses too: going ahead would be the skip
flag with nothing verified behind it."
```

---

### Task 7: the smoke script takes an agy machine

**Files:**
- Modify: `scripts/smoke-profiles.sh` (header comment, the argument check, the final calls)
- Modify: `Makefile:144-146`

- [ ] **Step 1: Edit the script**

In the header comment, change the usage line to `make smoke-profiles REPO='~/src/pastor' CLAUDE=<machine> OPENCODE=<machine> AGY=<machine>` and the sentence after it to `CLAUDE, OPENCODE and AGY name machines from flock.toml; leave one out to skip that agent; CLAUDE_AGENT, OPENCODE_AGENT and AGY_AGENT pick another agent of that kind`. Add after it: `An AGY machine's own profile must be review, with review's rules in its agy settings (see the manual's Permission profiles).`

Change the argument check to:

```sh
if [ -z "${CLAUDE:-}" ] && [ -z "${OPENCODE:-}" ] && [ -z "${AGY:-}" ]; then
    echo "smoke-profiles: set CLAUDE, OPENCODE and/or AGY to a machine name from flock.toml" >&2
    exit 2
fi
```

Add after the opencode call at the end:

```sh
[ -n "${AGY:-}" ] && smoke "${AGY_AGENT:-agy}" "$AGY"
```

- [ ] **Step 2: Edit the Makefile**

Change the target's help and command to:

```make
smoke-profiles: ## live review task per agent: make smoke-profiles REPO='~/src/x' CLAUDE=m1 OPENCODE=m2 AGY=m3
	REPO="$(REPO)" CLAUDE="$(CLAUDE)" OPENCODE="$(OPENCODE)" AGY="$(AGY)" scripts/smoke-profiles.sh
```

and the `smoke-rc` recipe's environment line (Makefile:153) to pass `AGY="$(AGY)"` too. In `scripts/smoke-rc.sh:146`, where it forwards `OPENCODE="${OPENCODE:-}"`, forward `AGY="${AGY:-}"` the same way.

- [ ] **Step 3: Check the script parses and the no-arg path**

Run: `sh -n scripts/smoke-profiles.sh && REPO=x scripts/smoke-profiles.sh; echo "exit $?"`
Expected: no syntax error; `smoke-profiles: set CLAUDE, OPENCODE and/or AGY ...` and `exit 2`.

- [ ] **Step 4: Commit**

```bash
git add scripts/smoke-profiles.sh scripts/smoke-rc.sh Makefile
git commit -m "test(smoke): a live review task on agy too

The suite starts no real agent, so the live check for a profile change
is this script; agy now has a profile to check."
```

---

### Task 8: docs, skill, AGENTS.md, change file

**Files:**
- Modify: `docs/manual.md` (table row 844, error table after line 342, Permission profiles around lines 2110-2138, smoke paragraph around line 4156)
- Modify: `skills/pastor/SKILL.md` (table row 64, bullets around lines 112-116)
- Modify: `AGENTS.md` (Known gaps, after line 234)
- Create: `changes/feat-agent-profiles.md`

- [ ] **Step 1: The Agent kinds table, manual and skill**

In both files replace the agy cell of the **Profile** row with:

```
`--dangerously-skip-permissions --mode accept-edits`, so it never asks; its settings on the machine must deny what the profile denies, checked before launch (`agy_permissions_missing`), and an agy machine runs one profile, its own (`agy_profile_mismatch`); a shell command can still write, so `review` is not a read-only checkout. `own_permissions = true` adds and checks nothing
```

In the manual, after the table's "See [How it works]..." paragraph, add:

```
A `kind` that is none of these four gets the lists through its `allow_flag`
and `deny_flag` and no never-ask mode, and refuses a list without them
(`agent_tools_unsupported`).
```

- [ ] **Step 2: The error codes table**

After the `agent_tools_unsupported` row add, keeping the table's alignment:

```
| `agy_deny_unsupported` | a profiled agy task denies a pattern agy has no rule for |
| `agy_permissions_missing` | a profiled agy task on a machine whose agy settings do not deny what the profile denies, or cannot be read |
| `agy_profile_mismatch` | a profiled agy task on a machine whose own profile does not deny everything the task's does |
```

- [ ] **Step 3: Permission profiles**

In the manual, replace the paragraph that begins `An agent whose permissions are in its own settings on the machine, such as agy with an allow list in its settings, says so with `own_permissions`:` and its toml fence and the paragraph after it (through `so set it only once every machine runs a release that has it.`) with:

````
An agy agent (kind `agy`, the Antigravity CLI) takes no policy at launch:
its rules live in its settings on the machine, `permissions.allow`,
`permissions.deny` and `permissions.ask` in `~/.gemini/antigravity-cli/settings.json`
and in the config it shares with the desktop app,
`~/.gemini/config/config.json`, and nothing on its command line or in its
environment carries rules. So a task under a profile starts with
`--dangerously-skip-permissions --mode accept-edits` after its args, which
approves every `ask` and skips the diff review before a file write; a
`deny` still holds under them. Before it makes anything on the machine,
pastor reads those two files there and checks that each pattern the task
denies is covered by a deny: `Edit`, `Write` and `NotebookEdit` need
`write_file(*)`, `Bash(git push:*)` needs `command(git push)` (a prefix by
words, so `command(git)` covers it too), `Read(~/.ssh/**)` needs
`read_file(~/.ssh)`, `WebFetch` needs `read_url(*)`, and `mcp__a__b` needs
`mcp(a/b)`; the action's `*` covers all of it, and a `read_file` deny of a
path covers a `write_file` of it. A `regex:` rule, an `ask` and an `allow`
cover nothing. What is missing fails the task before it starts
(`agy_permissions_missing` in its `error`), with the rules to add, one per
line. So does a machine whose files pastor cannot read, or that takes no
shell command (a `command` machine): unlike opencode's check, this one never
goes ahead, since going ahead would be the skip flag with nothing verified
behind it. A pattern agy has no rule for (`Glob`, `Grep`, `WebSearch`,
`Task`, an unknown tool) is refused when the task is queued
(`agy_deny_unsupported`). agy's project files under
`~/.gemini/config/projects/` are not read: agy applies only the one for
its active workspace, which pastor cannot tell, so a deny there does not
count.

What the profile allows is not passed: agy has no allow list to take, and
under the flag everything its settings do not deny runs, a shell command
that writes a file included. A `review` task on agy therefore never asks,
never uses an edit tool, never runs `git push`, `rm -rf` or `sudo`; it is
not a read-only checkout, as a Codex one under `review` is not either.
`pastor task describe` shows the lists with `(agy takes no lists: the deny
was checked against its settings on the machine, the allow is not passed)`
after them. Agent args that set `--mode` or `--dangerously-skip-permissions`
are refused while a profile applies (`profile_args_conflict`).

Those rules are one file per machine, so an agy machine runs one profile at
a time: `review` needs `write_file(*)` denied, which stops every edit there,
the owner's own agy sessions included. Give each agy machine its profile in
its `[[machine]]` entry (or its flock's, or `[defaults]`), write that
profile's rules into its agy settings, and run its tasks under that
profile. A task that asks for a profile the machine's own does not cover
(the task denies a pattern the machine's profile does not) is refused when
pinned there (`agy_profile_mismatch`), and skips that machine when not; a
machine with no own profile is tried, and the check above decides. For the
built-in `review` the deny list on the machine needs at least:

```json
{
  "permissions": {
    "deny": ["write_file(*)", "command(git push)", "command(rm -rf)", "command(sudo)"]
  }
}
```

and `develop` the last three without `write_file(*)`. Writing them is a
hand step today; the provisioner work takes it over.

An agent whose permissions pastor should leave alone says so with
`own_permissions`:

```toml
# pastor.toml
[agents.agy-builder]
kind = "agy"
own_permissions = true
```

A task of that agent under a profile starts with its args alone: pastor
adds no permission flag and no mode, runs no check, and the profile's lists
are not passed. The agent's own settings on the machine decide what it may
run, so they must hold at least what the profile would, and agy with it
asks before whatever they do not allow. `pastor task describe` shows the
lists with `(not applied: agent agy-builder's permissions are in its own
settings on the machine)` after them. The field cannot stand beside an
`allow_flag` or `deny_flag`, and without a profile the lists still need
flags. The unrestricted rule below holds for it as for any agent. To give
it to some machines only, define it under its own name and name it on those
machines (`agent = "agy-builder"`, or `agents = { agy = "agy-builder" }`
for a model of kind agy).
````

Also change, in the same section, the earlier sentence about which agents refuse lists if it says agy needs `own_permissions` (search the section for `own_permissions` and `agy`), and in the smoke paragraph near line 4156 change `(\`CLAUDE\` and \`OPENCODE\` take a machine from flock.toml, \`CLAUDE_AGENT\` and \`OPENCODE_AGENT\` another agent of that kind)` to `(\`CLAUDE\`, \`OPENCODE\` and \`AGY\` take a machine from flock.toml, \`CLAUDE_AGENT\`, \`OPENCODE_AGENT\` and \`AGY_AGENT\` another agent of that kind; the agy machine's own profile must be \`review\`, with its rules in agy's settings there)`. Update the `make smoke-profiles` line in the Makefile listing (line ~4131) to add `AGY=pi-3`.

- [ ] **Step 4: The skill's profile bullets**

In `skills/pastor/SKILL.md`, after the opencode bullet under `--profile NAME`, add:

```
  - An agy agent starts with `--dangerously-skip-permissions --mode accept-edits` and never asks; its settings on the machine must deny what the profile denies, checked before launch (`agy_permissions_missing` names the rules to add), and an agy machine runs one profile, its own (`agy_profile_mismatch` pinned elsewhere; unpinned it skips the machine). A shell command can still write, so `review` on agy is not a read-only checkout. A deny agy has no rule for is `agy_deny_unsupported`.
```

In the "Tool permissions" bullet replace `unless under a profile its `[agents.<name>]` table says `own_permissions = true`: then its own settings on the machine hold (agy's allow list), pastor passes no flag or mode, and `task describe` marks the lists not applied.` with `unless under a profile its `[agents.<name>]` table says `own_permissions = true`: then pastor passes no flag or mode and checks nothing, its own settings on the machine hold, and `task describe` marks the lists not applied.`

- [ ] **Step 5: AGENTS.md known gap and the change file**

Append to Known gaps in `AGENTS.md`:

```
- agy's `artifactReviewPolicy` defaults to `asks-for-review`, which can
  stop an interactive session to review a plan artifact; pastor's dialog
  reading does not know that prompt. A machine that runs agy tasks should
  set it to `agent-decides` or `always-proceed`. And agy's rules are one
  file per machine, so an agy machine runs one profile; writing that
  profile's rules there is a hand step until the provisioner work lands.
```

Write `changes/feat-agent-profiles.md`:

```markdown
### Added

- Permission profiles apply to agy agents. A task under a profile starts
  agy with `--dangerously-skip-permissions --mode accept-edits`, so it never
  stops at a prompt, and before launch pastor reads agy's settings on the
  machine and checks they deny what the profile denies, failing the task
  with the rules to add otherwise (`agy_permissions_missing`). A deny
  pattern agy has no rule for is refused when queued
  (`agy_deny_unsupported`). agy's rules are per machine, so a profiled agy
  task runs only on a machine whose own profile denies everything the
  task's does (`agy_profile_mismatch`); unpinned, it skips the others.
  `make smoke-profiles` takes `AGY=<machine>`.

### Changed

- `own_permissions = true` on an agy agent is no longer needed for a
  profile; it still means pastor adds and checks nothing.
```

- [ ] **Step 6: Run the gate**

Run: `make check`
Expected: PASS, including the doc tests that check every command and flag named in the manual and the skill, and that every `toml` and `json` fence parses. A failure there names the line; fix the wording, not the test.

- [ ] **Step 7: Commit**

```bash
git add docs/manual.md skills/pastor/SKILL.md AGENTS.md changes/feat-agent-profiles.md
git commit -m "docs: permission profiles on agy

The manual says what a profile on agy promises and what it does not, how
the settings on the machine are checked, and that an agy machine runs one
profile; the skill and the Agent kinds table follow."
```

---

### Task 9: pull request

**Files:** none.

- [ ] **Step 1: Final gate and push**

Run: `make check && git push`
Expected: PASS; the branch `feat/agent-profiles` is on origin.

- [ ] **Step 2: Open the pull request**

```bash
gh pr create --base main --title "feat: permission profiles for agy" --body-file - <<'EOF'
A task under a permission profile on an agy agent now never stops at a prompt and never does what the profile denies, or is refused with a code that says why.

- agy takes no policy at launch, so a profiled task starts with `--dangerously-skip-permissions --mode accept-edits`, and before launch pastor reads agy's settings on the machine and checks they deny what the profile denies (`agy_permissions_missing`, with the rules to add).
- A deny agy has no rule for is refused at queue time (`agy_deny_unsupported`).
- agy's rules are per machine, so an agy machine runs one profile; a task skips a machine whose own profile does not cover its denies (`agy_profile_mismatch`).
- A shell command can still write under the flag; the manual says `review` on agy is not a read-only checkout, as for Codex.

Spec: `docs/superpowers/specs/2026-10-15-agy-profiles-design.md`. Plan: `docs/superpowers/plans/2026-10-15-agy-profiles.md`. An agy task (t-1201) reviewed the spec.

Owed on the fleet before trusting it: `make smoke-profiles REPO='~/src/pastor' AGY=<machine>` on a machine whose own profile is `review` with review's rules in its agy settings. Writing those rules onto a machine is the provisioner work (branch `feat/agy-provisioner`, in progress).
EOF
```

- [ ] **Step 3: Report**

Give the PR URL, say `make check` passed, and that `make smoke-profiles` with `AGY=` is owed on a fleet machine whose own profile is `review`.
