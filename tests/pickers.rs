//! `limit::limit_picker` against the end of a pane, one file per screen
//! under `tests/fixtures/pickers/`. Each file opens with `#!` header lines
//! naming the right answer:
//!
//! ```text
//! #! kind: claude           the agent's kind
//! #! source: real|spec|made
//! #! now: <rfc 3339>        when the pane is read
//! #! picker: stop|no_stop|none
//! #! keys: <key> ...        with `stop`: what picks "Stop and wait"
//! #! until: none | <rfc 3339>
//! ```
//!
//! and the pane follows; a screen with no picker leaves the last two out.
//!
//! `source`: `spec` is the picker as the spec lists it, written from memory
//! of the tool; `made` is written for the test. No real picker has been
//! captured yet. `tests/panes.rs` checks that these files are scrubbed.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pickers")
}

fn check(name: &str) {
    let path = dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut header = std::collections::BTreeMap::new();
    let mut rest = text.as_str();
    while let Some(line) = rest.strip_prefix("#! ") {
        let (line, tail) = line.split_once('\n').unwrap_or((line, ""));
        rest = tail;
        let (key, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("{name}: header line without ':': {line}"));
        assert!(
            header.insert(key.trim(), value.trim()).is_none(),
            "{name}: {key} given twice"
        );
    }
    let mut take = |key: &str| {
        header
            .remove(key)
            .unwrap_or_else(|| panic!("{name}: no {key} header"))
    };
    let source = take("source");
    assert!(
        ["real", "spec", "made"].contains(&source),
        "{name}: source: {source:?}"
    );
    let kind = take("kind");
    let now: DateTime<Utc> = take("now").parse().expect("now");
    let want = take("picker");
    let picker = pastor::limit::limit_picker(kind, rest, now);
    for other in ["opencode", "codex"] {
        assert_eq!(
            pastor::limit::limit_picker(other, rest, now),
            None,
            "{name} as {other}"
        );
    }
    if want == "none" {
        assert_eq!(picker, None, "{name}: no picker on this screen");
        assert!(header.is_empty(), "{name}: unknown headers {header:?}");
        return;
    }
    let picker = picker.unwrap_or_else(|| panic!("{name}: no picker read"));
    let keys = picker.stop_keys();
    match want {
        "stop" => {
            let want: Vec<String> = take("keys").split_whitespace().map(String::from).collect();
            assert_eq!(keys, Some(want), "{name}: keys");
        }
        "no_stop" => assert_eq!(keys, None, "{name}: nothing to press"),
        v => panic!("{name}: picker: want stop, no_stop or none, got {v:?}"),
    }
    // Whatever the picker, the keys never reach the options that spend
    // money or change the plan.
    for key in keys.iter().flatten() {
        assert!(
            ["Up", "Down", "Enter"].contains(&key.as_str()),
            "{name}: {key}"
        );
    }
    let until = match take("until") {
        "none" => None,
        t => Some(t.parse::<DateTime<Utc>>().expect("until")),
    };
    assert_eq!(picker.limit.until, until, "{name}: until");
    assert!(picker.limit.hard, "{name}: a picker is a hard limit");
    assert!(header.is_empty(), "{name}: unknown headers {header:?}");
}

/// One test per fixture. `every_fixture_has_a_test` keeps this list and the
/// directory in step.
macro_rules! pickers {
    ($($test:ident: $file:literal,)*) => {
        $(
            #[test]
            fn $test() {
                check($file);
            }
        )*
        const FILES: &[&str] = &[$($file),*];
    };
}

pickers! {
    picker: "claude-picker.txt",
    picker_in_another_order: "claude-picker-reordered.txt",
    picker_with_the_cursor_below_stop: "claude-picker-cursor-below.txt",
    picker_without_stop_and_wait: "claude-picker-no-stop.txt",
    picker_in_unknown_words: "claude-picker-unknown-wording.txt",
    no_picker_at_a_permission_prompt: "no-picker-permission-prompt.txt",
    no_picker_quoted_in_a_message: "no-picker-quoted-in-message.txt",
    no_picker_above_the_last_prompt: "no-picker-above-the-last-prompt.txt",
}

#[test]
fn every_fixture_has_a_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(dir())
        .expect("tests/fixtures/pickers")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = FILES.iter().map(|f| f.to_string()).collect();
    listed.sort();
    assert_eq!(
        on_disk, listed,
        "tests/fixtures/pickers and pickers! differ"
    );
}
