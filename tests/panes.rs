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
//! parsers happen to say: a screen they read wrong keeps its right answer
//! and its test is `#[ignore]`d with the reason, until the parser is fixed.
//!
//! The repository is public, so the fixtures are scrubbed: `fixtures_are_scrubbed`
//! fails on a home path, an IPv4 address, this host's name or a machine of the
//! local flock.

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

fn check(name: &str) {
    let f = load(name);
    let question = pastor::task::trailing_question(&f.pane);
    assert_eq!(
        question.is_some(),
        f.question,
        "{name}: trailing_question gave {question:?}"
    );
    assert_eq!(
        pastor::task::background_shell_running(&f.pane),
        f.shell,
        "{name}: background_shell_running"
    );
    assert_eq!(
        pastor::config::shows_trust_marker(&f.pane, CLAUDE_TRUST_MARKER),
        f.trust,
        "{name}: shows_trust_marker"
    );
}

/// One test per fixture, so a screen read wrong can be ignored on its own.
/// `every_fixture_has_a_test` keeps this list and the directory in step.
macro_rules! panes {
    ($($(#[$attr:meta])* $test:ident: $file:literal,)*) => {
        $(
            #[test]
            $(#[$attr])*
            fn $test() {
                check($file);
            }
        )*
        const FILES: &[&str] = &[$($file),*];
    };
}

panes! {
    numbered_options_then_question: "numbered-options-then-question.txt",
    #[ignore = "trailing_question reads only the last paragraph; options after the question hide it"]
    question_then_numbered_options: "question-then-numbered-options.txt",
    question_in_bold: "question-in-bold.txt",
    question_then_tool_call: "question-then-tool-call.txt",
    #[ignore = "trailing_question counts tool output under a `●` call as the agent speaking"]
    tool_output_ending_in_question_mark: "tool-output-ending-in-question-mark.txt",
    #[ignore = "trailing_question cannot tell a code block from prose"]
    code_block_ending_in_question_mark: "code-block-ending-in-question-mark.txt",
    one_shell_running_wrapped_footer: "one-shell-running-wrapped-footer.txt",
    #[ignore = "background_shell_running wants the count on the same line as the phrase"]
    one_shell_running_split_count: "one-shell-running-split-count.txt",
    two_shells_running: "two-shells-running.txt",
    shell_phrase_quoted_in_message: "shell-phrase-quoted-in-message.txt",
    trust_dialog_fresh: "trust-dialog-fresh.txt",
    trust_dialog_in_scrollback: "trust-dialog-in-scrollback.txt",
    opencode_finished: "opencode-finished.txt",
    #[ignore = "trailing_question needs the message's `●` line, which a 100-line read cut off"]
    cut_mid_message_at_100_lines: "cut-mid-message-at-100-lines.txt",
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
/// ssh user and ssh host of the local flock (`PASTOR_CONFIG_DIR`, else
/// `~/.config/pastor`), when there is one. On CI there is none, and only the
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
    let config = std::env::var_os("PASTOR_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|d| d.join("pastor")));
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
    for name in FILES {
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
