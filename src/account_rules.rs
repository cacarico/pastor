//! A Claude account's own `ask` and `deny` rules that undo what a permission
//! profile allows. A task under a profile starts with `--allowedTools`, but
//! Claude applies the account's `settings.json` as well, and an `ask` rule
//! there wins over an allow: under a profile a task never prompts, so the
//! tool is refused, with no prompt and nothing in the task's record of why. A
//! `deny` there wins too. `pastor machine check` reads each Claude agent's
//! settings and warns, once per agent, naming the rule and the allow it
//! shadows. It changes nothing.
//!
//! An agy account is read too. agy takes no lists at launch: a task under a
//! profile starts in `always-proceed`, where every `ask` is approved and
//! every `deny` holds, and the agy provisioner writes the machine's own
//! profile into agy's global settings before each launch (`config::agy`).
//! So what an agy account can still hold against a profile's allow is a
//! `deny` in the config agy shares with the desktop app, which pastor never
//! writes: the tool is refused there, `pastor task done` included.

use std::time::Duration;

use crate::config::agy;
use crate::config::profile::Profile;
use crate::herdr::transport::shell_quote;
use crate::provision::run::{AgentEnv, Row, Shell, Status};

/// The id of the row each Claude and agy agent gets.
pub const ROW: &str = "account-rules";

/// The herdr kind this applies to.
const KIND: &str = "claude";

/// The env key that moves a Claude account's config directory, and what it
/// is without one.
const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const DEFAULT_DIR: &str = "~/.claude";

/// The line the read prints before its answer, so whatever the machine's rc
/// files print first is skipped.
const MARK: &str = "pastor-account-rules-5d27";

/// A settings file larger than this is cut, and read as not JSON.
const MAX_SETTINGS: usize = 200_000;

const READ_TIME: Duration = Duration::from_secs(30);

/// One account rule that shadows one profile allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadow {
    /// `ask` or `deny`.
    pub list: &'static str,
    /// The account's rule, as written.
    pub rule: String,
    /// The profile's allow it shadows, as written.
    pub allow: String,
}

/// A tool pattern split into what it matches: `tool`, then the text of its
/// parenthesis, cut at a `*` or a trailing `:*` into a literal start and a
/// flag for "and whatever follows". `None` for the argument of a pattern
/// with none (all of the tool).
struct Pattern<'a> {
    tool: &'a str,
    arg: Option<(String, bool)>,
}

fn parse(pattern: &str) -> Pattern<'_> {
    let pattern = pattern.trim();
    let (tool, rest) = match pattern.split_once('(') {
        Some((tool, rest)) => (
            tool.trim(),
            Some(rest.strip_suffix(')').unwrap_or(rest).trim()),
        ),
        None => (pattern, None),
    };
    let arg = rest.filter(|a| !a.is_empty() && *a != "*").map(|a| {
        let a = a
            .strip_suffix(":*")
            .map(|b| (b, true))
            .unwrap_or((a, false));
        match a.0.find('*') {
            Some(at) => (a.0[..at].to_string(), true),
            None => (a.0.to_string(), a.1),
        }
    });
    Pattern { tool, arg }
}

/// The tools Claude's `Edit` rules apply to as one.
fn edit_tool(tool: &str) -> bool {
    matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
}

/// Does `broad` match everything `narrow` does? The same tool (the editing
/// ones count as one; an MCP server's name covers its tools), and an
/// argument that is none, or a start the other begins with, or the same.
/// What it cannot tell it answers no.
fn covers(broad: &str, narrow: &str) -> bool {
    let (b, n) = (parse(broad), parse(narrow));
    let same_tool = b.tool == n.tool
        || (edit_tool(b.tool) && edit_tool(n.tool))
        || (b.tool.starts_with("mcp__")
            && n.tool
                .strip_prefix(b.tool)
                .is_some_and(|rest| rest.starts_with("__")));
    if !same_tool {
        return false;
    }
    match (&b.arg, &n.arg) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some((bl, true)), Some((nl, _))) => nl.starts_with(bl.as_str()),
        (Some((bl, false)), Some((nl, false))) => bl == nl,
        (Some((_, false)), Some((_, true))) => false,
    }
}

/// Do the two match something in common: one covers the other.
fn overlaps(a: &str, b: &str) -> bool {
    covers(a, b) || covers(b, a)
}

/// A settings file's `text` as its top-level object, or why it is not one.
fn settings(text: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not JSON ({e})"))?;
    match root {
        serde_json::Value::Object(root) => Ok(root),
        _ => Err("its top level is not an object".into()),
    }
}

/// The strings of `permissions.<list>` in `root`; anything else adds none.
fn listed<'a>(
    root: &'a serde_json::Map<String, serde_json::Value>,
    list: &str,
) -> impl Iterator<Item = &'a str> {
    root.get("permissions")
        .and_then(|p| p.get(list))
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str())
}

/// The rules in an account's settings `text` that shadow an allow of the
/// profile: each `ask` or `deny` rule that overlaps an allow and that the
/// profile's own deny does not already cover (a rule the profile denies
/// anyway costs the task nothing new). A rule is reported once, against the
/// first allow it overlaps. An error says why the
/// text is not settings.
pub fn shadows(text: &str, profile: &Profile) -> Result<Vec<Shadow>, String> {
    let root = settings(text)?;
    let mut out = Vec::new();
    for list in ["ask", "deny"] {
        for rule in listed(&root, list) {
            if profile.deny.iter().any(|d| covers(d, rule)) {
                continue;
            }
            if let Some(allow) = profile.allow.iter().find(|a| overlaps(rule, a)) {
                out.push(Shadow {
                    list: if list == "ask" { "ask" } else { "deny" },
                    rule: rule.to_string(),
                    allow: allow.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// Do `rule` and `allow`, both agy rules, match something in common: one
/// covers the other. The other way round only within one action, since
/// agy's `read_file` deny covering a write is not an allow's to borrow.
fn agy_overlaps(rule: &str, allow: &str) -> bool {
    let action = |r: &str| r.split_once('(').map(|(a, _)| a.to_string());
    agy::covers(rule, allow) || (action(rule) == action(allow) && agy::covers(allow, rule))
}

/// The `deny` rules in agy's shared config `text` that shadow an allow of
/// the profile, or `pastor task done`, which every task under a profile is
/// allowed: each that overlaps the allow as agy reads it
/// (`agy::required_rule`) and that the profile's own deny does not already
/// cover. Its `ask` rules are approved under a profile, so none is read. A
/// rule is reported once, against the first allow it overlaps; an allow agy
/// has no rule for meets none. An error says why the text is not settings.
pub fn agy_shadows(text: &str, profile: &Profile) -> Result<Vec<Shadow>, String> {
    let root = settings(text)?;
    let denied = agy::required_rules(&profile.deny);
    let allows: Vec<(&str, String)> = profile
        .allow
        .iter()
        .map(String::as_str)
        .chain([crate::config::TASK_DONE_ALLOW])
        .filter_map(|p| agy::required_rule(p).map(|r| (p, r)))
        .collect();
    let mut out = Vec::new();
    for rule in listed(&root, "deny") {
        if denied.iter().any(|d| agy::covers(d, rule)) {
            continue;
        }
        if let Some((allow, _)) = allows.iter().find(|(_, a)| agy_overlaps(rule, a)) {
            out.push(Shadow {
                list: "deny",
                rule: rule.to_string(),
                allow: allow.to_string(),
            });
        }
    }
    Ok(out)
}

/// A path on the machine as a shell word: `~` is the machine's home, which
/// the word names through `$HOME`.
fn path_word(dir: &str) -> String {
    match dir.strip_prefix('~') {
        Some("") => "\"$HOME\"".to_string(),
        Some(rest) if rest.starts_with('/') => format!("\"$HOME\"{}", shell_quote(rest)),
        _ => shell_quote(dir),
    }
}

/// The shell command that prints, after `MARK`, `file` and the settings
/// file at `path`, or `none` when there is none.
fn read_command(path: &str) -> String {
    format!(
        "f={}; printf '%s\\n' {MARK}; if [ -r \"$f\" ]; then echo file; head -c {MAX_SETTINGS} \"$f\"; else echo none; fi",
        path_word(path)
    )
}

/// Reads the read command's stdout: the file's text, `None` when there is no
/// file.
fn parse_read(stdout: &str) -> Result<Option<String>, String> {
    let line = format!("{MARK}\n");
    let at = stdout
        .match_indices(&line)
        .map(|(i, _)| i)
        .find(|&i| i == 0 || stdout.as_bytes()[i - 1] == b'\n')
        .ok_or("no answer from the settings read")?;
    let rest = &stdout[at + line.len()..];
    let (status, text) = rest.split_once('\n').unwrap_or((rest, ""));
    match status.trim() {
        "file" => Ok(Some(text.to_string())),
        "none" => Ok(None),
        other => Err(format!(
            "unexpected answer from the settings read: {other:?}"
        )),
    }
}

fn row(agent: &str, status: Status, first_line: String, reason: Option<String>) -> Row {
    Row {
        id: ROW.into(),
        agent: Some(agent.into()),
        status,
        first_line,
        reason,
    }
}

/// One row for each Claude and agy agent of `agents`, about its account's
/// settings on the machine behind `shell` and `profile`, the one the
/// machine's tasks get: a Claude account's `settings.json`, under its
/// `CLAUDE_CONFIG_DIR`, and agy's shared config. `ok` when no rule shadows
/// an allow, `warn` naming the rules when one does, and `warn` too when the
/// settings cannot be read.
pub async fn rows(shell: &dyn Shell, profile: &Profile, agents: &[AgentEnv]) -> Vec<Row> {
    let mut out = Vec::new();
    for agent in agents {
        let (file, lists, find): (String, &str, fn(&str, &Profile) -> _) = if agent.kind == KIND {
            let dir = agent
                .env
                .get(CONFIG_DIR_ENV)
                .map(String::as_str)
                .unwrap_or(DEFAULT_DIR);
            (format!("{dir}/settings.json"), "ask or deny", shadows)
        } else if agent.kind == agy::KIND {
            (agy::SHARED_CONFIG.to_string(), "deny", agy_shadows)
        } else {
            continue;
        };
        let name = &agent.name;
        let ran = shell
            .run(read_command(&file), Vec::new(), READ_TIME)
            .await
            .map_err(|e| format!("could not read {file}: {e}"))
            .and_then(|ran| {
                parse_read(&String::from_utf8_lossy(&ran.stdout))
                    .map_err(|e| format!("could not read {file}: {e}"))
            });
        let text = match ran {
            Ok(Some(text)) => text,
            Ok(None) => {
                out.push(row(name, Status::Pass, format!("no {file}"), None));
                continue;
            }
            Err(why) => {
                out.push(row(name, Status::Warn, why, None));
                continue;
            }
        };
        match find(&text, profile) {
            Err(why) => out.push(row(name, Status::Warn, format!("{file} is {why}"), None)),
            Ok(found) if found.is_empty() => out.push(row(
                name,
                Status::Pass,
                format!("no {lists} rule shadows profile {}'s allows", profile.name),
                None,
            )),
            Ok(found) => {
                let say = |s: &Shadow| {
                    format!(
                        "{} {} in {file} shadows profile {}'s allow {}",
                        s.list, s.rule, profile.name, s.allow
                    )
                };
                let rest: Vec<String> = found[1..].iter().map(say).collect();
                out.push(row(
                    name,
                    Status::Warn,
                    say(&found[0]),
                    (!rest.is_empty()).then(|| rest.join("; ")),
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profile::Profiles;
    use crate::provision::run::testing::Fake;

    fn profile(text: &str, name: &str) -> Profile {
        toml::from_str::<Profiles>(text)
            .unwrap()
            .resolve(name)
            .unwrap()
    }

    fn develop() -> Profile {
        Profiles::default().resolve("develop").unwrap()
    }

    /// An `ask` over an allow, the stall a headless task cannot answer, is
    /// named with the allow it shadows, whichever of the two is narrower.
    #[test]
    fn an_ask_over_an_allow_is_named_with_both() {
        let found = shadows(
            r#"{"permissions":{"ask":["Bash(git push:*)","WebFetch"],"allow":["Bash(ls)"]}}"#,
            &develop(),
        )
        .unwrap();
        assert_eq!(
            found,
            [Shadow {
                list: "ask",
                rule: "Bash(git push:*)".into(),
                allow: "Bash".into(),
            }]
        );
        // The ask is the broader one.
        let p = profile("[p]\nallow = [\"Bash(git push:*)\", \"Read\"]\n", "p");
        let found = shadows(r#"{"permissions":{"ask":["Bash"]}}"#, &p).unwrap();
        assert_eq!(found[0].allow, "Bash(git push:*)");
        // Edit and Write are one family for Claude's rules.
        let found = shadows(r#"{"permissions":{"ask":["Write(src/**)"]}}"#, &develop()).unwrap();
        assert_eq!(
            (found[0].rule.as_str(), found[0].allow.as_str()),
            ("Write(src/**)", "Edit")
        );
    }

    /// A clean home, rules that overlap no allow, and a deny the profile has
    /// too, give nothing.
    #[test]
    fn a_clean_account_shadows_nothing() {
        for settings in [
            "{}",
            r#"{"permissions":{}}"#,
            r#"{"permissions":{"allow":["Bash(git push:*)"],"ask":[],"deny":[]}}"#,
            r#"{"permissions":{"ask":["WebFetch","WebSearch"]}}"#,
            // An ask the profile denies anyway: Claude's deny wins over it.
            r#"{"permissions":{"ask":["Bash(sudo:*)","Bash(sudo -n true)","Bash(rm -rf:*)"]}}"#,
            // The profile denies it already: nothing new is lost.
            r#"{"permissions":{"deny":["Bash(rm -rf:*)","Bash(sudo:*)","Bash(sudo -n true)"]}}"#,
        ] {
            let found = shadows(settings, &develop()).unwrap();
            assert!(found.is_empty(), "{settings}: {found:?}");
        }
    }

    /// A deny the profile does not have narrows an allow, and is named.
    #[test]
    fn a_deny_the_profile_lacks_is_named() {
        let found = shadows(
            r#"{"permissions":{"deny":["Read(./.env)","Bash(rm -rf:*)","mcp__srv__tool"]}}"#,
            &profile("[p]\nallow = [\"Read\", \"mcp__srv\"]\n", "p"),
        )
        .unwrap();
        let named: Vec<(&str, &str, &str)> = found
            .iter()
            .map(|s| (s.list, s.rule.as_str(), s.allow.as_str()))
            .collect();
        assert_eq!(
            named,
            [
                ("deny", "Read(./.env)", "Read"),
                ("deny", "mcp__srv__tool", "mcp__srv"),
            ]
        );
    }

    #[test]
    fn patterns_overlap_by_tool_and_start() {
        for (a, b, yes) in [
            ("Bash", "Bash(git push:*)", true),
            ("Bash(git:*)", "Bash(git push:*)", true),
            ("Bash(git push:*)", "Bash(git:*)", true),
            ("Bash(git push)", "Bash(git push:*)", true),
            ("Bash(git push)", "Bash(git pull)", false),
            ("Bash(git log:*)", "Bash(git push:*)", false),
            ("Bash(npm run *)", "Bash(npm run build)", true),
            ("Read(~/.ssh/**)", "Read(~/.ssh/id)", true),
            ("Read(~/.ssh/**)", "Read(src/**)", false),
            ("Read", "Edit", false),
            ("Edit", "NotebookEdit(a.ipynb)", true),
            ("mcp__srv", "mcp__srv__tool", true),
            ("mcp__srv", "mcp__srvx__tool", false),
            ("WebFetch(domain:a.org)", "WebFetch(domain:b.org)", false),
        ] {
            assert_eq!(overlaps(a, b), yes, "{a} / {b}");
        }
    }

    #[test]
    fn settings_that_are_not_settings_say_so() {
        assert!(
            shadows("{ nope", &develop())
                .unwrap_err()
                .contains("not JSON")
        );
        assert!(
            shadows("[]", &develop())
                .unwrap_err()
                .contains("not an object")
        );
    }

    fn agent(name: &str, kind: &str, dir: Option<&str>) -> AgentEnv {
        AgentEnv {
            name: name.into(),
            kind: kind.into(),
            env: dir
                .map(|d| [(CONFIG_DIR_ENV.to_string(), d.to_string())].into())
                .unwrap_or_default(),
        }
    }

    fn home_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Fake) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        for (path, text) in files {
            let file = home.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        std::fs::create_dir_all(&home).unwrap();
        let fake = Fake::new(&home, None);
        (tmp, fake)
    }

    /// Each Claude agent gets a row about its own account's settings, found
    /// through its `CLAUDE_CONFIG_DIR` (`~` is the machine's home) or
    /// `~/.claude`; a codex one gets none. A home with an ask over a profile
    /// allow warns, naming both; a clean one passes, and so does one with
    /// no settings file.
    #[tokio::test]
    async fn each_claude_account_gets_a_row() {
        let (_tmp, fake) = home_with(&[
            (
                ".claude-main/settings.json",
                r#"{"permissions":{"ask":["Bash(git push:*)","Edit(docs/**)"]}}"#,
            ),
            (
                ".claude/settings.json",
                r#"{"permissions":{"allow":["Bash"]}}"#,
            ),
        ]);
        let agents = [
            agent("claude", "claude", None),
            agent("claude-main", "claude", Some("~/.claude-main")),
            agent("claude-new", "claude", Some("~/.claude-new")),
            agent("codex", "codex", None),
        ];
        let rows = rows(&fake, &develop(), &agents).await;
        let got: Vec<(&str, Status)> = rows
            .iter()
            .map(|r| (r.agent.as_deref().unwrap(), r.status))
            .collect();
        assert_eq!(
            got,
            [
                ("claude", Status::Pass),
                ("claude-main", Status::Warn),
                ("claude-new", Status::Pass),
            ]
        );
        assert!(rows.iter().all(|r| r.id == ROW));
        assert_eq!(
            rows[0].first_line,
            "no ask or deny rule shadows profile develop's allows"
        );
        assert_eq!(
            rows[1].first_line,
            "ask Bash(git push:*) in ~/.claude-main/settings.json shadows profile develop's allow Bash"
        );
        assert_eq!(
            rows[1].reason.as_deref(),
            Some(
                "ask Edit(docs/**) in ~/.claude-main/settings.json shadows profile develop's allow Edit"
            )
        );
        assert_eq!(rows[2].first_line, "no ~/.claude-new/settings.json");
    }

    /// A settings file that is not JSON is a warning, not a pass.
    #[tokio::test]
    async fn broken_settings_warn() {
        let (_tmp, fake) = home_with(&[(".claude/settings.json", "{ nope")]);
        let rows = rows(&fake, &develop(), &[agent("claude", "claude", None)]).await;
        assert_eq!(rows[0].status, Status::Warn);
        assert!(rows[0].first_line.contains("not JSON"), "{:?}", rows[0]);
    }

    #[test]
    fn the_read_answer_skips_what_the_rc_files_print() {
        let out = format!("welcome\n{MARK}\nfile\n{{\"a\":1}}\n");
        assert_eq!(parse_read(&out).unwrap().as_deref(), Some("{\"a\":1}\n"));
        assert_eq!(parse_read(&format!("{MARK}\nnone\n")).unwrap(), None);
        assert!(parse_read("motd\n").is_err());
        assert_eq!(path_word("~/.claude-main"), "\"$HOME\"'/.claude-main'");
        assert_eq!(path_word("~"), "\"$HOME\"");
        assert_eq!(path_word("/srv/it's"), "'/srv/it'\\''s'");
    }

    /// An agy `deny` over an allow is named with the allow, as agy reads
    /// both: the profile's `Bash` is every command, its `Edit` every write.
    /// A deny the profile has too, one of a tool the profile does not allow
    /// and an `ask`, which a task under a profile approves, are not.
    #[test]
    fn an_agy_deny_over_an_allow_is_named() {
        let found = agy_shadows(
            r#"{"permissions":{"deny":["command(git push)","command(sudo)","read_url(example.org)","write_file(/etc)"],"ask":["command(make)"]}}"#,
            &develop(),
        )
        .unwrap();
        let got: Vec<(&str, &str, &str)> = found
            .iter()
            .map(|s| (s.list, s.rule.as_str(), s.allow.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("deny", "command(git push)", "Bash"),
                ("deny", "write_file(/etc)", "Edit"),
            ]
        );
    }

    /// `pastor task done` is allowed under every profile, so a deny over it
    /// is named even when the profile allows no such command. agy's
    /// `read_file` deny blocks writes too, but a `write_file` deny does not
    /// touch a `Read` allow.
    #[test]
    fn an_agy_deny_over_task_done_is_named() {
        let review = Profiles::default().resolve("review").unwrap();
        let found = agy_shadows(
            r#"{"permissions":{"deny":["command(pastor)","command(curl)"]}}"#,
            &review,
        )
        .unwrap();
        assert_eq!(
            found,
            [Shadow {
                list: "deny",
                rule: "command(pastor)".into(),
                allow: crate::config::TASK_DONE_ALLOW.into(),
            }]
        );
        let only_reads = profile("[only-reads]\nallow = [\"Read\"]\n", "only-reads");
        let found = agy_shadows(
            r#"{"permissions":{"deny":["write_file(/etc)"]}}"#,
            &only_reads,
        )
        .unwrap();
        assert_eq!(found, [], "a write deny is not over a read allow");
    }

    /// Each agy agent gets a row about agy's shared config, the one file of
    /// its rules pastor does not write: a deny there over an allow warns,
    /// naming the file, the rule and the allow, and what the global
    /// settings say is not read as the account's. A clean config passes,
    /// and so does a machine with none; one that is not JSON warns.
    #[tokio::test]
    async fn an_agy_account_gets_a_row_from_its_shared_config() {
        let shared = ".gemini/config/config.json";
        let global = ".gemini/antigravity-cli/settings.json";
        let (_tmp, fake) = home_with(&[
            (shared, r#"{"permissions":{"deny":["command(git push)"]}}"#),
            (global, r#"{"permissions":{"deny":["command(git)"]}}"#),
        ]);
        let rows = super::rows(&fake, &develop(), &[agent("agy", "agy", None)]).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (
                rows[0].agent.as_deref(),
                rows[0].status,
                rows[0].id.as_str()
            ),
            (Some("agy"), Status::Warn, ROW)
        );
        assert_eq!(
            rows[0].first_line,
            "deny command(git push) in ~/.gemini/config/config.json shadows profile develop's allow Bash"
        );
        assert_eq!(rows[0].reason, None);

        let (_tmp, fake) = home_with(&[
            (
                shared,
                r#"{"permissions":{"deny":["command(sudo)"]},"theme":"dark"}"#,
            ),
            (global, r#"{"permissions":{"deny":["command(git)"]}}"#),
        ]);
        let rows = super::rows(&fake, &develop(), &[agent("agy", "agy", None)]).await;
        assert_eq!(rows[0].status, Status::Pass, "{rows:?}");
        assert_eq!(
            rows[0].first_line,
            "no deny rule shadows profile develop's allows"
        );

        let (_tmp, fake) = home_with(&[(global, r#"{"permissions":{"deny":["command(git)"]}}"#)]);
        let rows = super::rows(&fake, &develop(), &[agent("agy", "agy", None)]).await;
        assert_eq!(
            (rows[0].status, rows[0].first_line.as_str()),
            (Status::Pass, "no ~/.gemini/config/config.json")
        );

        let (_tmp, fake) = home_with(&[(shared, "{ nope")]);
        let rows = super::rows(&fake, &develop(), &[agent("agy", "agy", None)]).await;
        assert_eq!(rows[0].status, Status::Warn);
        assert!(
            rows[0]
                .first_line
                .starts_with("~/.gemini/config/config.json is not JSON"),
            "{:?}",
            rows[0]
        );
    }
}
