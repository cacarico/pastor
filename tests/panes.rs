//! The pane heuristics (`task::trailing_question`,
//! `task::background_shell_running`, `config::shows_trust_marker`) against
//! pane tails laid out as herdr's `agent.read` gives them
//! (`recent_unwrapped`, 100 lines), one file per screen under
//! `tests/fixtures/panes/`. Each file opens with `#!` header lines naming the
//! right answer:
//!
//! ```text
//! #! question: yes|no   the last message ends on a question (blocked)
//! #! shell: yes|no      the footer says a background shell is running
//! #! trust: yes|no      the prompt at the bottom is Claude's trust dialog
//! ```
//!
//! and the pane follows. The header gives the right answer, not what the
//! parsers happen to say: a screen a parser reads wrong keeps its right
//! answer and that parser's test for it is `#[ignore]`d with the reason,
//! until the parser is fixed.
//!
//! The repository is public, so the fixtures are scrubbed: `fixtures_are_scrubbed`
//! fails on a home path, an IPv4 address, this host's name or a machine of the
//! local flock, here and in `tests/fixtures/limits/` (`tests/limits.rs`).

use std::path::{Path, PathBuf};

/// What `pastor` looks for by default on a Claude pane
/// (`config::CLAUDE_TRUST_MARKER`, private there).
const CLAUDE_TRUST_MARKER: &str = "Yes, I trust this folder";

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/panes")
}

struct Fixture {
    question: bool,
    shell: bool,
    trust: bool,
    pane: String,
}

fn load(name: &str) -> Fixture {
    let path = dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut question = None;
    let mut shell = None;
    let mut trust = None;
    let mut rest = text.as_str();
    while let Some(line) = rest.strip_prefix("#! ") {
        let (line, tail) = line.split_once('\n').unwrap_or((line, ""));
        rest = tail;
        let (key, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("{name}: header line without ':': {line}"));
        let value = match value.trim() {
            "yes" => true,
            "no" => false,
            v => panic!("{name}: {key}: want yes or no, got {v:?}"),
        };
        let slot = match key.trim() {
            "question" => &mut question,
            "shell" => &mut shell,
            "trust" => &mut trust,
            k => panic!("{name}: unknown header {k:?}"),
        };
        assert!(slot.replace(value).is_none(), "{name}: {key} given twice");
    }
    let need = |v: Option<bool>, key: &str| v.unwrap_or_else(|| panic!("{name}: no {key} header"));
    Fixture {
        question: need(question, "question"),
        shell: need(shell, "shell"),
        trust: need(trust, "trust"),
        pane: rest.to_string(),
    }
}

fn check_question(name: &str) {
    let f = load(name);
    let question = pastor::task::trailing_question(&f.pane);
    assert_eq!(
        question.is_some(),
        f.question,
        "{name}: trailing_question gave {question:?}"
    );
}

fn check_shell(name: &str) {
    let f = load(name);
    assert_eq!(
        pastor::task::background_shell_running(&f.pane),
        f.shell,
        "{name}: background_shell_running"
    );
}

fn check_trust(name: &str) {
    let f = load(name);
    assert_eq!(
        pastor::config::shows_trust_marker(&f.pane, CLAUDE_TRUST_MARKER),
        f.trust,
        "{name}: shows_trust_marker"
    );
}

/// One module per fixture and one test per heuristic in it, so a screen one
/// parser reads wrong is ignored for that parser only and the other two keep
/// guarding it. `every_fixture_has_a_test` keeps this list and the directory
/// in step.
macro_rules! panes {
    ($(
        $test:ident: $file:literal {
            $(#[$q:meta])* question,
            $(#[$s:meta])* shell,
            $(#[$t:meta])* trust,
        },
    )*) => {
        $(
            mod $test {
                #[test]
                $(#[$q])*
                fn question() {
                    super::check_question($file);
                }

                #[test]
                $(#[$s])*
                fn shell() {
                    super::check_shell($file);
                }

                #[test]
                $(#[$t])*
                fn trust() {
                    super::check_trust($file);
                }
            }
        )*
        const FILES: &[&str] = &[$($file),*];
    };
}

panes! {
    numbered_options_then_question: "numbered-options-then-question.txt" {
        question, shell, trust,
    },
    question_then_numbered_options: "question-then-numbered-options.txt" {
        #[ignore = "trailing_question reads only the last paragraph; options after the question hide it"]
        question,
        shell,
        trust,
    },
    question_in_bold: "question-in-bold.txt" {
        question, shell, trust,
    },
    question_then_tool_call: "question-then-tool-call.txt" {
        question, shell, trust,
    },
    tool_output_ending_in_question_mark: "tool-output-ending-in-question-mark.txt" {
        #[ignore = "trailing_question counts tool output under a `●` call as the agent speaking"]
        question,
        shell,
        trust,
    },
    code_block_ending_in_question_mark: "code-block-ending-in-question-mark.txt" {
        #[ignore = "trailing_question cannot tell a code block from prose"]
        question,
        shell,
        trust,
    },
    one_shell_running_wrapped_footer: "one-shell-running-wrapped-footer.txt" {
        question, shell, trust,
    },
    one_shell_running_split_count: "one-shell-running-split-count.txt" {
        question,
        #[ignore = "background_shell_running wants the count on the same line as the phrase"]
        shell,
        trust,
    },
    two_shells_running: "two-shells-running.txt" {
        question, shell, trust,
    },
    shell_phrase_quoted_in_message: "shell-phrase-quoted-in-message.txt" {
        question, shell, trust,
    },
    trust_dialog_fresh: "trust-dialog-fresh.txt" {
        question, shell, trust,
    },
    trust_dialog_in_scrollback: "trust-dialog-in-scrollback.txt" {
        question, shell, trust,
    },
    opencode_finished: "opencode-finished.txt" {
        question, shell, trust,
    },
    cut_mid_message_at_100_lines: "cut-mid-message-at-100-lines.txt" {
        #[ignore = "trailing_question needs the message's `●` line, which a 100-line read cut off"]
        question,
        shell,
        trust,
    },
}

#[test]
fn every_fixture_has_a_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(dir())
        .expect("tests/fixtures/panes")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = FILES.iter().map(|f| f.to_string()).collect();
    listed.sort();
    assert_eq!(on_disk, listed, "tests/fixtures/panes and panes! differ");
}

/// The cut-off fixture is the length pastor reads (`agent_read(target, 100)`).
#[test]
fn the_cut_fixture_is_100_lines() {
    assert_eq!(
        load("cut-mid-message-at-100-lines.txt")
            .pane
            .lines()
            .count(),
        100
    );
}

/// Words of `text`: runs of the characters a host or user name is made of.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .map(|w| w.trim_matches('.'))
        .filter(|w| !w.is_empty())
}

fn is_ipv4(word: &str) -> bool {
    let parts: Vec<&str> = word.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
}

/// Names the fixtures must not carry: this host's, and every machine name,
/// ssh user and ssh host of the local flock, found where pastor itself looks
/// (`Paths::from_env`), when there is one. On CI there is none, and only the
/// fixed checks run.
fn fleet_names() -> Vec<String> {
    let mut names = Vec::new();
    for host in [
        std::env::var("HOSTNAME").ok(),
        std::fs::read_to_string("/etc/hostname").ok(),
    ]
    .into_iter()
    .flatten()
    {
        names.extend(words(&host).map(str::to_string));
    }
    let config = pastor::config::Paths::from_env().ok().map(|p| p.config_dir);
    let flock = config.and_then(|d| std::fs::read_to_string(d.join("flock.toml")).ok());
    if let Some(value) = flock.and_then(|t| t.parse::<toml::Table>().ok()) {
        for machine in value
            .get("machine")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
        {
            for key in ["name", "ssh"] {
                if let Some(s) = machine.get(key).and_then(|v| v.as_str()) {
                    names.extend(words(s).map(str::to_string));
                }
            }
        }
    }
    // A name as short as `pi` would match ordinary words.
    names.retain(|n| n.len() >= 3);
    names.sort();
    names.dedup();
    names
}

#[test]
fn fixtures_are_scrubbed() {
    let fleet = fleet_names();
    // The screens of `tests/limits.rs` are published with these.
    let limits = dir().with_file_name("limits");
    let limits: Vec<String> = std::fs::read_dir(&limits)
        .expect("tests/fixtures/limits")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .map(|name| format!("../limits/{name}"))
        .collect();
    assert!(!limits.is_empty(), "no fixture under tests/fixtures/limits");
    for name in FILES
        .iter()
        .copied()
        .chain(limits.iter().map(String::as_str))
    {
        let text = std::fs::read_to_string(dir().join(name)).expect("fixture");
        for path in ["/home/", "/Users/", "/root/"] {
            assert!(!text.contains(path), "{name}: contains {path}");
        }
        for word in words(&text) {
            assert!(!is_ipv4(word), "{name}: contains an IPv4 address");
            assert!(
                !fleet.iter().any(|f| f.eq_ignore_ascii_case(word)),
                "{name}: contains a fleet host or user name"
            );
        }
    }
}

#[test]
fn the_scrub_check_catches_what_it_should() {
    assert!(is_ipv4("192.168.1.20"));
    assert!(!is_ipv4("0.5.0") && !is_ipv4("1.2.3.999"));
    assert_eq!(
        words("ssh user@pi-1.lan, done.").collect::<Vec<_>>(),
        ["ssh", "user", "pi-1.lan", "done"]
    );
}
