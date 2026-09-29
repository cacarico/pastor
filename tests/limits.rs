//! `limit::limit_in` against the end of a pane as herdr's `agent.read` gives
//! it, one file per screen under `tests/fixtures/limits/`. Each file opens
//! with `#!` header lines naming the right answer:
//!
//! ```text
//! #! kind: claude         the agent's kind
//! #! source: real|spec|made
//! #! now: <rfc 3339>      when the pane is read
//! #! limit: hard|short|none
//! #! model_scoped: yes|no
//! #! until: none | <rfc 3339> | local [<weekday> | <date>] <HH:MM>
//! ```
//!
//! and the pane follows; a screen with no limit leaves the last two out.
//! `until: local ...` is for a message that names no time zone, which is
//! read in the zone of the machine the test runs on: the next such time of
//! day, the next such weekday, or that day.
//!
//! `source` says where the message comes from. `real`: Claude Code 2.1.281
//! wrote it in a transcript when the account reached its limit, and only the
//! time zone's name was changed; for agy, agy's own log held it from a
//! print-mode run at its quota, word for word. `spec`: from the list in the spec, written
//! from memory of the tool. `made`: written for the test. The pane around
//! the message is laid out like the fixtures of `tests/panes.rs` in every
//! case; none is a captured screen.
//!
//! `tests/panes.rs` checks that these files are scrubbed too.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, Timelike, Utc, Weekday};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/limits")
}

#[derive(Debug, PartialEq)]
enum Until {
    None,
    At(DateTime<Utc>),
    /// The next such time of day in local time.
    Local(NaiveTime),
    /// The next such weekday in local time.
    LocalWeekday(Weekday, NaiveTime),
    LocalDate(NaiveDate, NaiveTime),
}

struct Fixture {
    kind: String,
    now: DateTime<Utc>,
    /// `Some(hard)`.
    limit: Option<bool>,
    model_scoped: bool,
    until: Until,
    pane: String,
}

fn yes_no(name: &str, key: &str, value: &str) -> bool {
    match value {
        "yes" => true,
        "no" => false,
        v => panic!("{name}: {key}: want yes or no, got {v:?}"),
    }
}

fn until(name: &str, value: &str) -> Until {
    fn read<T: std::str::FromStr>(name: &str, value: &str) -> T {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name}: until: cannot read {value:?}"))
    }
    let clock = |s: &str| {
        NaiveTime::parse_from_str(s, "%H:%M")
            .unwrap_or_else(|_| panic!("{name}: until: cannot read {s:?}"))
    };
    if value == "none" {
        return Until::None;
    }
    let Some(local) = value.strip_prefix("local ") else {
        return Until::At(read(name, value));
    };
    match local.split_once(' ') {
        None => Until::Local(clock(local)),
        Some((day, time)) => match day.parse::<Weekday>() {
            Ok(weekday) => Until::LocalWeekday(weekday, clock(time)),
            Err(_) => Until::LocalDate(read(name, day), clock(time)),
        },
    }
}

fn load(name: &str) -> Fixture {
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
        "{name}: source: want real, spec or made, got {source:?}"
    );
    let limit = match take("limit") {
        "hard" => Some(true),
        "short" => Some(false),
        "none" => None,
        v => panic!("{name}: limit: want hard, short or none, got {v:?}"),
    };
    let fixture = Fixture {
        kind: take("kind").to_string(),
        now: take("now")
            .parse()
            .unwrap_or_else(|e| panic!("{name}: now: {e}")),
        limit,
        model_scoped: limit.is_some() && yes_no(name, "model_scoped", take("model_scoped")),
        until: match limit {
            Some(_) => until(name, take("until")),
            None => Until::None,
        },
        pane: rest.to_string(),
    };
    assert!(header.is_empty(), "{name}: unknown headers {header:?}");
    fixture
}

fn check(name: &str) {
    let f = load(name);
    let limit = pastor::limit::limit_in(&f.kind, &f.pane, f.now);
    let Some(hard) = f.limit else {
        assert_eq!(limit, None, "{name}: no limit on this screen");
        return;
    };
    let limit = limit.unwrap_or_else(|| panic!("{name}: no limit read"));
    assert_eq!(limit.hard, hard, "{name}: hard, from {:?}", limit.line);
    assert_eq!(
        limit.model_scoped, f.model_scoped,
        "{name}: model_scoped, from {:?}",
        limit.line
    );
    assert!(
        f.pane.contains(&limit.line) && !limit.line.is_empty(),
        "{name}: line {:?} is not in the pane",
        limit.line
    );
    let local = limit.until.map(|t| t.with_timezone(&Local));
    let at = |time: NaiveTime| {
        let local = local.unwrap_or_else(|| panic!("{name}: no until read"));
        assert_eq!(
            (local.hour(), local.minute()),
            (time.hour(), time.minute()),
            "{name}: until {local}"
        );
        assert!(local > f.now, "{name}: until {local} is not after now");
        local
    };
    match f.until {
        Until::None => assert_eq!(limit.until, None, "{name}: until"),
        Until::At(t) => assert_eq!(limit.until, Some(t), "{name}: until"),
        Until::Local(time) => {
            let local = at(time);
            assert!(
                local.with_timezone(&Utc) - f.now <= chrono::Duration::hours(25),
                "{name}: until {local} is not the next {time}"
            );
        }
        Until::LocalWeekday(weekday, time) => {
            let local = at(time);
            assert_eq!(local.weekday(), weekday, "{name}: until {local}");
            assert!(
                local.with_timezone(&Utc) - f.now <= chrono::Duration::days(7),
                "{name}: until {local} is not the next {weekday}"
            );
        }
        Until::LocalDate(date, time) => {
            assert_eq!(at(time).date_naive(), date, "{name}: until");
        }
    }
}

/// One test per fixture. `every_fixture_has_a_test` keeps this list and the
/// directory in step.
macro_rules! limits {
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

limits! {
    session_limit: "claude-session-limit.txt",
    weekly_limit: "claude-weekly-limit.txt",
    model_limit: "claude-model-limit.txt",
    session_limit_under_the_prompt: "claude-session-limit-under-prompt.txt",
    limit_with_a_named_zone: "claude-limit-named-zone.txt",
    five_hour_limit: "claude-five-hour-limit.txt",
    five_hour_limit_on_a_24h_clock: "claude-five-hour-limit-24h-clock.txt",
    weekly_limit_with_a_weekday: "claude-weekly-limit-weekday.txt",
    opus_weekly_limit_with_a_date: "claude-opus-weekly-limit-date.txt",
    usage_limit_with_a_unix_time: "claude-usage-limit-unix-time.txt",
    credit_too_low: "claude-credit-too-low.txt",
    api_error_529: "claude-api-error-529.txt",
    api_error_429: "claude-api-error-429.txt",
    limit_banner: "claude-limit-banner.txt",
    no_limit_above_the_last_prompt: "no-limit-above-the-last-prompt.txt",
    no_limit_in_a_grep_of_the_parser: "no-limit-grep-of-the-parser.txt",
    no_limit_quoted_in_the_last_message: "no-limit-quoted-in-the-last-message.txt",
    no_limit_before_the_last_message: "no-limit-before-the-last-message.txt",
    no_limit_while_claude_retries: "no-limit-retrying.txt",
    agy_quota_reached: "agy-quota-reached.txt",
    agy_quota_reached_in_print_mode: "agy-quota-reached-print-mode.txt",
    agy_rate_limit: "agy-rate-limit.txt",
    no_agy_limit_above_the_last_prompt: "no-limit-agy-above-the-last-prompt.txt",
    no_agy_limit_before_the_last_message: "no-limit-agy-before-the-last-message.txt",
    no_agy_limit_in_a_grep_of_the_parser: "no-limit-agy-grep-of-the-parser.txt",
}

#[test]
fn every_fixture_has_a_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(dir())
        .expect("tests/fixtures/limits")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = FILES.iter().map(|f| f.to_string()).collect();
    listed.sort();
    assert_eq!(on_disk, listed, "tests/fixtures/limits and limits! differ");
}

/// Every fixture is a Claude or an agy screen, and the same screen under
/// another kind reads as no limit: each kind reads only its own messages.
#[test]
fn another_kind_reads_no_limit() {
    for name in FILES {
        let f = load(name);
        assert!(["claude", "agy"].contains(&f.kind.as_str()), "{name}");
        for kind in ["claude", "agy", "opencode", "codex", "claude-personal"] {
            if kind == f.kind {
                continue;
            }
            assert_eq!(
                pastor::limit::limit_in(kind, &f.pane, f.now),
                None,
                "{name} as {kind}"
            );
        }
    }
}
