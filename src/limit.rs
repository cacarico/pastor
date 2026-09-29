//! Usage limits, read from the end of an agent's turn.
//!
//! An agent that stopped on a usage limit is idle, exactly like one that
//! finished, so the text it left is the only place the difference shows.
//! `limit_in` reads that text, and is strict about where it looks: pastor's
//! own source, tests and notes carry the messages it looks for, and an agent
//! that greps them must not read as limited. Claude's and agy's messages
//! are known; any other kind reads as no limit.

use std::time::Duration;

use chrono::{DateTime, Datelike, Local, Month, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};

use serde::{Deserialize, Serialize};

use crate::config::LimitsConfig;
use crate::task::MESSAGE_MARKERS;

/// How long to wait after a limit whose message names no reset time, before
/// trying again.
pub const UNKNOWN_RESET_WAIT: Duration = Duration::from_secs(3600);

/// How many lines of a turn are read, counted from its end, empty ones
/// left out. A limit is the last thing its agent shows.
const TURN_LINES: usize = 15;

/// What starts the output of a tool call, and Claude's own notices under a
/// prompt.
const OUTPUT_MARKER: char = '⎿';

/// A limit an agent stopped on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit {
    /// A usage limit, or no credit. `false` for a 429 or 529 that outlived
    /// the agent's own retries.
    pub hard: bool,
    /// When the limit resets, always after the `now` it was read at; `None`
    /// when the message names no time, or one that has passed.
    pub until: Option<DateTime<Utc>>,
    /// The limit is one model's (`Opus weekly limit reached`): the
    /// account's other models still work.
    pub model_scoped: bool,
    /// The account has no credit left (`Credit balance is too low`): no
    /// reset is coming, someone has to pay.
    #[serde(default)]
    pub no_credit: bool,
    /// The line it was read from, without its marker.
    pub line: String,
}

impl Limit {
    /// When to try the agent again: at the reset, or `UNKNOWN_RESET_WAIT`
    /// from `now` when the message named none.
    pub fn retry_at(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        self.until
            .unwrap_or(now + chrono::Duration::from_std(UNKNOWN_RESET_WAIT).expect("an hour fits"))
    }

    /// When the account is tried again, as `[limits]` says: at the reset,
    /// else `retry_after_no_credit` from `now` for no credit, else
    /// `unknown_reset_wait` from `now`.
    pub fn retry_at_under(&self, now: DateTime<Utc>, limits: &LimitsConfig) -> DateTime<Utc> {
        let wait = if self.no_credit {
            limits.retry_after_no_credit_duration()
        } else {
            limits.unknown_reset_wait_duration()
        };
        self.until.unwrap_or_else(|| {
            now.checked_add_signed(
                chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::MAX),
            )
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
        })
    }

    /// What ran out, in a few words: `5-hour limit`, `Opus weekly limit`,
    /// `no credit`, `rate limit`.
    pub fn what(&self) -> String {
        what_of(&self.line, self.hard, self.no_credit)
    }
}

/// `Limit::what` from a limit's parts, for a row that kept only those.
pub fn what_of(line: &str, hard: bool, no_credit: bool) -> String {
    if no_credit {
        return "no credit".into();
    }
    if !hard {
        return "rate limit".into();
    }
    let lower = line.replace('’', "'").to_ascii_lowercase();
    match scope_of(&lower) {
        Some(words) if !words.is_empty() => {
            // The words as the message wrote them, capitals kept. `lower`
            // and `line` differ in bytes (`’` is three, `'` one) but not
            // in words, so the scope is found by word index, not offset.
            // `words` borrow from `lower`, which gives where they start.
            let first = words[0].as_ptr() as usize - lower.as_ptr() as usize;
            let skip = lower[..first].split_whitespace().count();
            let said: Vec<&str> = line
                .split_whitespace()
                .skip(skip)
                .take(words.len())
                .collect();
            format!("{} limit", said.join(" "))
        }
        _ => "usage limit".into(),
    }
}

/// A limit the head keeps (`Store::record_limit`): an account, or one model
/// of it, that no task starts on before `retry_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountLimit {
    /// The agent's `account`, else `<machine>/<agent>` (`Agents::limit_key`).
    pub account: String,
    /// The `[models]` name the limit is for, when it is one model's; `None`
    /// stops every model of the account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub hard: bool,
    pub no_credit: bool,
    /// The reset the message named, if it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
    /// When the account is tried again: `until`, or a wait from `[limits]`.
    pub retry_at: DateTime<Utc>,
    /// The line the limit was read from.
    pub line: String,
    /// The task it was seen on, the machine and the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub seen_at: DateTime<Utc>,
}

impl AccountLimit {
    /// The row for `limit`, seen at `now` on `task`'s agent `agent` on
    /// `machine`, kept under `account`. A limit that is one model's keeps
    /// `model`, the task's `[models]` name; with none it stops the account.
    #[allow(clippy::too_many_arguments)]
    pub fn of(
        account: &str,
        model: Option<&str>,
        limit: &Limit,
        task_id: Option<i64>,
        machine: Option<&str>,
        agent: Option<&str>,
        now: DateTime<Utc>,
        limits: &LimitsConfig,
    ) -> AccountLimit {
        AccountLimit {
            account: account.to_string(),
            model: model.filter(|_| limit.model_scoped).map(str::to_string),
            hard: limit.hard,
            no_credit: limit.no_credit,
            until: limit.until,
            retry_at: limit.retry_at_under(now, limits),
            line: limit.line.clone(),
            task_id,
            machine: machine.map(str::to_string),
            agent: agent.map(str::to_string),
            seen_at: now,
        }
    }

    /// Whether the row stops `model` (a `[models]` name, or none).
    pub fn stops(&self, model: Option<&str>) -> bool {
        match &self.model {
            None => true,
            Some(m) => model == Some(m.as_str()),
        }
    }

    /// The account, and the model when the row is one model's:
    /// `claude-personal`, `claude-personal opus`.
    pub fn name(&self) -> String {
        match &self.model {
            Some(m) => format!("{} {m}", self.account),
            None => self.account.clone(),
        }
    }

    /// What ran out, as `Limit::what` says it.
    pub fn what(&self) -> String {
        what_of(&self.line, self.hard, self.no_credit)
    }

    /// The detail of `agent.exhausted` (`by` is `None`) or `agent.reset`
    /// (`by: time` or `hand`) about this row.
    pub fn event_detail(&self, by: Option<&str>) -> serde_json::Value {
        let mut detail = serde_json::json!({
            "account": self.account,
            "model": self.model,
            "agent": self.agent,
            "retry_at": self.retry_at,
        });
        if let Some(obj) = detail.as_object_mut() {
            match by {
                Some(by) => {
                    obj.insert("by".into(), by.into());
                }
                None => {
                    obj.insert("until".into(), serde_json::json!(self.until));
                    obj.insert("hard".into(), self.hard.into());
                    obj.insert("no_credit".into(), self.no_credit.into());
                    obj.insert("what".into(), self.what().into());
                    obj.insert("line".into(), self.line.clone().into());
                }
            }
        }
        detail
    }

    /// `claude-personal exhausted until 03:00`: `note` without what ran out.
    pub fn short_note(&self, now: DateTime<Utc>) -> String {
        format!(
            "{} exhausted until {}",
            self.name(),
            local_time(self.retry_at, now)
        )
    }

    /// `claude-personal exhausted until 03:00 (5-hour limit, seen by t-412)`:
    /// why a task does not start on it, with `retry_at` in local time.
    pub fn note(&self, now: DateTime<Utc>) -> String {
        let seen = self
            .task_id
            .map(|id| format!(", seen by t-{id}"))
            .unwrap_or_default();
        format!(
            "{} exhausted until {} ({}{seen})",
            self.name(),
            local_time(self.retry_at, now),
            self.what(),
        )
    }
}

/// `at` in local time as a person reads it next to `now`: `03:00` today,
/// `Tue 03:00` within the week, `Oct 6 03:00` further off.
pub fn local_time(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let at = at.with_timezone(&Local);
    let now = now.with_timezone(&Local);
    if at.date_naive() == now.date_naive() {
        at.format("%H:%M").to_string()
    } else if (at - now).num_days().abs() < 6 {
        at.format("%a %H:%M").to_string()
    } else {
        at.format("%b %-d %H:%M").to_string()
    }
}

/// The limit an agent of `kind` stopped on, if `text` (the end of its pane,
/// or its task's error) ends on one.
///
/// For Claude the turn is what follows the last prompt that was sent (a line
/// starting with `>` or `❯`; the input box, which sits under a rule, is not
/// one), or the whole text when it shows none. Of that, only the last
/// `TURN_LINES` lines that are not empty are read, and a line counts only
/// when it starts with the message:
///
/// - the last `●` message, on its first line;
/// - a `⎿` notice that is not the output of a tool call;
/// - a line in the first column, which is the raw message (`claude -p`, a
///   task's error);
/// - a line under the input box, which is the limit banner.
///
/// So a message further up, one quoted in the middle of a line, and tool
/// output that shows one (a grep of this file) are not limits.
///
/// agy draws no marker before its messages, so for agy the end of the turn
/// is its last paragraph (`agy_limit`).
pub fn limit_in(kind: &str, text: &str, now: DateTime<Utc>) -> Option<Limit> {
    match kind {
        "claude" => claude_limit(text, now),
        "agy" => agy_limit(text, now),
        _ => None,
    }
}

/// The lines of a paragraph of agy's that are read, the first one and the
/// ones a long message wrapped onto.
const AGY_MESSAGE_LINES: usize = 4;

/// The limit agy stopped on, if the last paragraph of its turn starts with
/// one of its messages. The turn is what follows the last prompt that was
/// sent (a line starting with `>`), above its input box (the `>` line
/// inside a box drawn with `╭` and `╰`) and the footer under it; the whole
/// text when it shows neither, as agy's print mode and a task's error do.
/// Paragraphs are split on empty lines, and the box's edges are not part of
/// one.
fn agy_limit(text: &str, now: DateTime<Utc>) -> Option<Limit> {
    let text = text.replace('\u{a0}', " ");
    let lines: Vec<&str> = text.lines().collect();
    let unboxed = |line: &str| {
        let line = line.trim();
        line.strip_prefix('│')
            .unwrap_or(line)
            .trim_start()
            .to_string()
    };
    let is_prompt = |line: &str| {
        let line = unboxed(line);
        line.strip_prefix('>')
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    // An input box waiting for input has no command text after `>`, only
    // whitespace and the box's right border; a sent prompt drawn inside a
    // box has text there instead.
    let is_empty_prompt = |line: &str| {
        let line = unboxed(line);
        line.strip_prefix('>')
            .is_some_and(|rest| rest.trim_matches([' ', '│']).is_empty())
    };
    let is_edge = |line: &str| line.trim_start().starts_with(['╭', '╰']);
    let prompts: Vec<usize> = (0..lines.len()).filter(|i| is_prompt(lines[*i])).collect();
    let input_box = prompts.last().copied().filter(|i| {
        is_empty_prompt(lines[*i])
            && lines[..*i]
                .iter()
                .rev()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| l.trim_start().starts_with('╭'))
    });
    let end = input_box.map_or(lines.len(), |b| {
        (0..b).rev().find(|i| is_edge(lines[*i])).unwrap_or(b)
    });
    let sent = prompts
        .iter()
        .rev()
        .find(|i| Some(**i) != input_box && **i < end);
    let start = sent.map_or(0, |i| i + 1);
    let turn: Vec<&str> = lines[start..end]
        .iter()
        .copied()
        .filter(|l| !is_edge(l))
        .collect();
    let last = turn.iter().rposition(|l| !l.trim().is_empty())?;
    let first = turn[..last]
        .iter()
        .rposition(|l| l.trim().is_empty())
        .map_or(0, |i| i + 1);
    let paragraph: Vec<&str> = turn[first..=last]
        .iter()
        .take(AGY_MESSAGE_LINES)
        .map(|l| l.trim())
        .collect();
    read_agy(paragraph[0], &paragraph[1..].join(" "), now)
}

/// The limit `said` starts with, if it starts with one of agy's messages:
/// an API status and its code, `RESOURCE_EXHAUSTED (code 429): ...`. A
/// `RESOURCE_EXHAUSTED` or a 429 is a limit; it is hard when it says the
/// quota is reached, a short one otherwise. `more` is what the message goes
/// on to say on its next lines.
fn read_agy(said: &str, more: &str, now: DateTime<Utc>) -> Option<Limit> {
    let (status, rest) = said.split_once(" (code ")?;
    if status.is_empty() || !status.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        return None;
    }
    let code: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if status != "RESOURCE_EXHAUSTED" && code != "429" {
        return None;
    }
    let whole = format!("{said} {more}");
    let lower = whole.to_ascii_lowercase();
    // While agy retries, the agent is at work and the turn goes on.
    if lower.contains("retrying") {
        return None;
    }
    Some(Limit {
        hard: lower.contains("quota reached"),
        until: reset_in(&whole, now),
        model_scoped: false,
        no_credit: false,
        line: said.to_string(),
    })
}

fn claude_limit(text: &str, now: DateTime<Utc>) -> Option<Limit> {
    let text = text.replace('\u{a0}', " ");
    let lines: Vec<&str> = text.lines().collect();
    let is_prompt = |i: usize| {
        let line: &str = lines[i];
        ['>', '❯']
            .iter()
            .filter_map(|m| line.strip_prefix(*m))
            .any(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    let prompts: Vec<usize> = (0..lines.len()).filter(|i| is_prompt(*i)).collect();
    let input_box = prompts
        .last()
        .copied()
        .filter(|i| *i > 0 && is_rule(lines[i - 1]));
    let sent = prompts.iter().rev().find(|i| Some(**i) != input_box);
    let start = sent.map_or(0, |i| i + 1);
    // What the person is typing can run over several lines; the footer
    // starts under the rule that closes the box.
    let footer = input_box.map(|b| {
        (b + 1..lines.len())
            .find(|i| is_rule(lines[*i]))
            .unwrap_or(b)
    });
    let turn: Vec<usize> = (start..lines.len())
        .filter(|i| !lines[*i].trim().is_empty())
        .collect();
    for &i in turn.iter().rev().take(TURN_LINES) {
        let line = lines[i];
        if Some(i) == input_box || is_rule(line) {
            continue;
        }
        let in_footer = footer.is_some_and(|f| i > f);
        let message = line.strip_prefix(MESSAGE_MARKERS);
        let said = if in_footer {
            line
        } else if let Some(rest) = message {
            rest
        } else if let Some(rest) = line
            .strip_prefix("  ")
            .and_then(|l| l.strip_prefix(OUTPUT_MARKER))
        {
            if under_a_tool_call(&lines, i) {
                continue;
            }
            rest
        } else if line.starts_with(char::is_whitespace) {
            continue;
        } else {
            line
        };
        // The reset can be on the next line of a message that wrapped.
        let more = if in_footer {
            String::new()
        } else {
            lines[i + 1..]
                .iter()
                .take_while(|l| l.starts_with(' ') && !l.trim().is_empty())
                .take(2)
                .map(|l| l.trim())
                .collect::<Vec<_>>()
                .join(" ")
        };
        if let Some(limit) = read_claude(said.trim(), &more, now) {
            return Some(limit);
        }
        if message.is_some() && !in_footer {
            // The last message, and it is not a limit: what is above it is
            // not the end of the turn.
            return None;
        }
    }
    None
}

/// A row of box-drawing dashes, as Claude draws above and under its input
/// box.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 3 && line.chars().all(|c| matches!(c, '─' | '━' | '╌' | '═'))
}

/// Whether the `⎿` on line `at` is the output of a tool call: the block it
/// is in, read upwards to an empty line, starts with a `●` line.
fn under_a_tool_call(lines: &[&str], at: usize) -> bool {
    lines[..at]
        .iter()
        .rev()
        .take_while(|l| !l.trim().is_empty())
        .find(|l| !l.starts_with(char::is_whitespace))
        .is_some_and(|l| l.starts_with(MESSAGE_MARKERS))
}

/// Words Claude puts between `your` and `limit` that name a window or the
/// account. Any other word there names a model.
const WINDOW_WORDS: [&str; 12] = [
    "5-hour", "ai", "claude", "current", "daily", "hourly", "monthly", "plan", "rate", "session",
    "usage", "weekly",
];

/// The limit `said` starts with, if it starts with one of Claude's
/// messages. `more` is what the message goes on to say on its next lines,
/// read for the reset only.
fn read_claude(said: &str, more: &str, now: DateTime<Utc>) -> Option<Limit> {
    let lower = said.replace('’', "'").to_ascii_lowercase();
    let mut no_credit = false;
    let (hard, model_scoped) = if let Some(rest) = lower.strip_prefix("api error: ") {
        // While Claude retries, the agent is at work and the turn goes on.
        let code: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !matches!(code.as_str(), "429" | "529") || lower.contains("retrying") {
            return None;
        }
        (false, false)
    } else if let Some(rest) = lower.strip_prefix("credit balance is too low") {
        if !ends_a_phrase(rest) {
            return None;
        }
        no_credit = true;
        (true, false)
    } else {
        (
            true,
            scope_of(&lower)?.iter().any(|w| !WINDOW_WORDS.contains(w)),
        )
    };
    let whole = format!("{said} {more}");
    Some(Limit {
        hard,
        until: reset_in(&whole, now),
        model_scoped,
        no_credit,
        line: said.to_string(),
    })
}

/// Whether `rest`, what follows a message's words, ends them: nothing, or
/// something that is not the next word of a sentence.
fn ends_a_phrase(rest: &str) -> bool {
    !rest.trim_start().starts_with(char::is_alphanumeric)
}

/// The words that say which limit `lower` is about, when it starts with
/// `you've hit your <words> limit`, `you've reached your <words> limit` or
/// `<words> limit reached`: none for `you've hit your limit`, `opus weekly`
/// for `opus weekly limit reached`. At most three words, and nothing but
/// words: a line of grep output (`12:    "usage limit reached",`) has none.
fn scope_of(lower: &str) -> Option<Vec<&str>> {
    let words: Vec<&str> = lower.split_whitespace().collect();
    let scope = if let ["you've", "hit" | "reached", "your", rest @ ..] = words.as_slice() {
        let at = rest
            .iter()
            .take(4)
            .position(|w| w.trim_end_matches(|c: char| !c.is_alphanumeric()) == "limit")?;
        let tail = rest[at + 1..].first().copied().unwrap_or("");
        if rest[at] == "limit" && !ends_a_phrase(tail) {
            return None;
        }
        &rest[..at]
    } else {
        let at = words.iter().take(4).position(|w| *w == "limit")?;
        let after = words.get(at + 1)?.strip_prefix("reached")?;
        let tail = words.get(at + 2).copied().unwrap_or("");
        if !ends_a_phrase(after) || (after.is_empty() && !ends_a_phrase(tail)) {
            return None;
        }
        &words[..at]
    };
    scope
        .iter()
        .all(|w| {
            w.chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '.')
        })
        .then(|| scope.to_vec())
}

/// The option of Claude's limit picker pastor picks. It is the only one:
/// the others spend money (extra usage) or change the plan, and that stays
/// a person's call.
pub const STOP_AND_WAIT: &str = "Stop and wait for limit to reset";

/// Words that make a picker option one of Claude's limit picker's.
const PICKER_WORDS: [&str; 3] = ["stop and wait", "extra usage", "upgrade your plan"];

/// Claude's picker at a usage limit (stop and wait, upgrade, or use extra
/// usage), which some builds show instead of ending the turn. herdr reports
/// the agent `blocked` on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitPicker {
    /// The options as the picker lists them, without their numbers.
    pub options: Vec<String>,
    /// The option the cursor is on.
    pub selected: usize,
    /// The limit it is about: the message above it when there is one, else
    /// a hard limit with the reset the picker names, if any.
    pub limit: Limit,
}

impl LimitPicker {
    /// The keys that pick `STOP_AND_WAIT` from where the cursor is, found by
    /// its text wherever it is in the list: `Up` or `Down` to it, then
    /// `Enter`. `None` when the picker has no such option; then nothing
    /// should be pressed.
    pub fn stop_keys(&self) -> Option<Vec<String>> {
        let want = STOP_AND_WAIT.to_ascii_lowercase();
        let at = self
            .options
            .iter()
            .position(|o| o.to_ascii_lowercase().starts_with(&want))?;
        let (key, n) = if at >= self.selected {
            ("Down", at - self.selected)
        } else {
            ("Up", self.selected - at)
        };
        let mut keys = vec![key.to_string(); n];
        keys.push("Enter".into());
        Some(keys)
    }

    /// `1. Upgrade your plan, 2. Use extra usage`: the options, for an
    /// error that says what the picker offered.
    pub fn listed(&self) -> String {
        self.options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("{}. {o}", i + 1))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Claude's limit picker, if an agent of `kind` shows one at the end of
/// `text`, its pane.
///
/// As strict as `limit_in`: the picker has to be the last thing on the
/// pane, under at most its key hints (`Enter to confirm · Esc to cancel`):
/// a run of options numbered from 1, one of them under the cursor (`❯`).
/// It is a limit picker when one of its options is one of Claude's limit
/// options (`PICKER_WORDS`), or when the text just above it is a limit
/// message. So a list in a message, which has no cursor, and another
/// dialog (a permission prompt) are not one.
pub fn limit_picker(kind: &str, text: &str, now: DateTime<Utc>) -> Option<LimitPicker> {
    match kind {
        "claude" => claude_picker(text, now),
        _ => None,
    }
}

fn claude_picker(text: &str, now: DateTime<Utc>) -> Option<LimitPicker> {
    let text = text.replace('\u{a0}', " ");
    let lines: Vec<&str> = text
        .lines()
        .map(unboxed)
        .filter(|l| !l.is_empty() && !is_rule(l) && !is_box_edge(l))
        .collect();
    let mut end = lines.len();
    // The key hints under the options.
    while end > 0 && is_key_hint(lines[end - 1]) && lines.len() - end < 2 {
        end -= 1;
    }
    let mut options = Vec::new();
    let mut selected = None;
    let mut start = end;
    while start > 0 {
        let Some((cursor, number, option)) = picker_option(lines[start - 1]) else {
            break;
        };
        if cursor {
            if selected.is_some() {
                return None;
            }
            selected = Some(start - 1);
        }
        options.push((number, option));
        start -= 1;
    }
    options.reverse();
    let numbered = options.iter().enumerate().all(|(i, (n, _))| *n == i + 1);
    if options.len() < 2 || !numbered {
        return None;
    }
    let selected = selected? - start;
    let options: Vec<String> = options.into_iter().map(|(_, o)| o.to_string()).collect();
    let above: Vec<&str> = lines[..start].iter().rev().take(4).copied().collect();
    let message = above.iter().find_map(|l| {
        let said = l
            .trim_start_matches(MESSAGE_MARKERS)
            .trim_start_matches(OUTPUT_MARKER)
            .trim();
        read_claude(said, "", now)
    });
    let ours = options.iter().any(|o| {
        let o = o.to_ascii_lowercase();
        PICKER_WORDS.iter().any(|w| o.contains(w))
    });
    if !ours && message.is_none() {
        return None;
    }
    let limit = message.unwrap_or_else(|| {
        let near: Vec<&str> = above.iter().rev().copied().collect();
        let near = near.join(" ");
        Limit {
            hard: true,
            until: reset_in(&near, now),
            model_scoped: false,
            no_credit: false,
            line: above
                .first()
                .map_or_else(|| STOP_AND_WAIT.to_string(), |l| l.to_string()),
        }
    });
    Some(LimitPicker {
        options,
        selected,
        limit,
    })
}

/// `line` without the sides of a box Claude draws around a dialog, and
/// without its indent.
fn unboxed(line: &str) -> &str {
    line.trim()
        .trim_start_matches('│')
        .trim_end_matches('│')
        .trim()
}

/// The top or bottom of a box: `╭────╮`.
fn is_box_edge(line: &str) -> bool {
    line.chars().count() >= 3
        && line
            .chars()
            .all(|c| matches!(c, '─' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘'))
}

/// A line of key hints under a picker: `Enter to confirm · Esc to cancel`.
fn is_key_hint(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ["to confirm", "to cancel", "to select", "to navigate"]
        .iter()
        .any(|h| lower.contains(h))
}

/// An option of a picker: `❯ 1. Stop and wait for limit to reset` or
/// `2. Upgrade your plan`, as (under the cursor, number, text).
fn picker_option(line: &str) -> Option<(bool, usize, &str)> {
    let (cursor, rest) = match line.strip_prefix(['❯', '>']) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, line),
    };
    let digits = rest.find(|c: char| !c.is_ascii_digit())?;
    let number = rest[..digits].parse().ok()?;
    let option = rest[digits..].strip_prefix(". ")?.trim();
    (!option.is_empty()).then_some((cursor, number, option))
}

/// When the limit `text` tells of resets, if it says: a unix time after
/// `|`, a time from now (`try again in 2 hours 13 minutes`, `in 20s`), or a
/// time of day after `reset` or `try again at`, alone (`resets 3am`,
/// `resets at 15:30`), on a weekday (`resets Mon 9am`) or on a date
/// (`resets Oct 6, 9am`), which is the next such time in the zone the
/// message names in brackets (`(Europe/Lisbon)`), else in local time. A
/// time that is not after `now` is not a reset.
pub fn reset_in(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = text.to_ascii_lowercase();
    if let Some((_, epoch)) = lower.split_once('|') {
        let digits: String = epoch.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(secs) = digits.parse::<i64>()
            && let Some(t) = Utc.timestamp_opt(secs, 0).single()
            && t > now
        {
            return Some(t);
        }
    }
    for key in ["try again in ", "retry in ", "resets in ", "reset in "] {
        if let Some(i) = lower.find(key)
            && let Some(wait) = duration_at(&lower[i + key.len()..])
        {
            return now.checked_add_signed(wait);
        }
    }
    for key in ["try again at", "resets", "reset"] {
        let Some(i) = lower.find(key) else { continue };
        let from = i + key.len();
        let Some(when) = When::at(&lower[from..]) else {
            continue;
        };
        // `lower` is as long as `text`, byte for byte; a zone's name needs
        // its capitals.
        return match zone_in(&text[from..]) {
            Some(zone) => when.next(&zone, now),
            None => when.next(&Local, now),
        };
    }
    None
}

/// The time zone `s` names in brackets, if it names one the tz database
/// knows.
fn zone_in(s: &str) -> Option<chrono_tz::Tz> {
    let (_, rest) = s.split_once('(')?;
    let (name, _) = rest.split_once(')')?;
    name.trim().parse().ok()
}

/// The wait at the start of `s`: `2 hours 13 minutes`, `4 days 3 hours`,
/// `20s`, `1.5s`.
fn duration_at(s: &str) -> Option<chrono::Duration> {
    let mut rest = s;
    let mut secs = 0.0;
    let mut found = false;
    loop {
        rest = rest.trim_start_matches([' ', ',']);
        rest = rest.strip_prefix("and ").unwrap_or(rest);
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let Ok(n) = rest[..digits].parse::<f64>() else {
            break;
        };
        let tail = rest[digits..].trim_start();
        let letters = tail
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(tail.len());
        let unit = match &tail[..letters] {
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
            "d" | "day" | "days" => 86_400.0,
            "w" | "week" | "weeks" => 604_800.0,
            _ => break,
        };
        secs += n * unit;
        found = true;
        rest = &tail[letters..];
    }
    // More than a year is not a wait anyone prints.
    (found && secs <= 366.0 * 86_400.0)
        .then(|| chrono::Duration::milliseconds((secs * 1000.0).round() as i64))
}

/// A reset as a message words it: a time of day, and the weekday or the
/// date it falls on when the message names one.
#[derive(Debug, PartialEq)]
struct When {
    weekday: Option<Weekday>,
    /// Month and day.
    date: Option<(u32, u32)>,
    time: Option<NaiveTime>,
}

impl When {
    /// What the start of `s` (what follows `reset` in a message, in lower
    /// case) says: `s 3am`, ` at 15:30`, `s mon 9am`, `s oct 6, 9am`.
    fn at(s: &str) -> Option<When> {
        let s = s.strip_prefix('s').unwrap_or(s);
        let mut rest = skip(s, &["at ", "on "]);
        let mut when = When {
            weekday: None,
            date: None,
            time: None,
        };
        let (word, tail) = word_at(rest);
        if let Ok(weekday) = word.parse() {
            when.weekday = Some(weekday);
            rest = skip(tail, &[","]);
        }
        let (word, tail) = word_at(rest);
        if let Ok(month) = word.parse::<Month>() {
            let tail = tail.trim_start();
            let digits = tail
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(tail.len());
            let day: u32 = tail[..digits].parse().ok()?;
            when.date = Some((month.number_from_month(), day));
            rest = skip(&tail[digits..], &["st", "nd", "rd", "th", ",", "at "]);
        }
        when.time = clock_at(skip(rest, &["at "]));
        (when.weekday.is_some() || when.date.is_some() || when.time.is_some()).then_some(when)
    }

    /// The next time after `now` that is this one in `zone`. A day with no
    /// time is its first minute. A time the clocks go back over is on the
    /// clock twice, and the second is still next once the first has passed.
    fn next<Z: TimeZone>(&self, zone: &Z, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let time = self.time.unwrap_or(NaiveTime::MIN);
        let at = |day: NaiveDate| {
            let (first, second) = match zone.from_local_datetime(&day.and_time(time)) {
                chrono::LocalResult::Single(t) => (Some(t), None),
                chrono::LocalResult::Ambiguous(a, b) => (Some(a), Some(b)),
                chrono::LocalResult::None => (None, None),
            };
            [first, second]
                .into_iter()
                .flatten()
                .map(|t| t.with_timezone(&Utc))
                .find(|t| *t > now)
        };
        let today = now.with_timezone(zone).date_naive();
        if let Some((month, day)) = self.date {
            return (today.year()..=today.year() + 1)
                .filter_map(|year| NaiveDate::from_ymd_opt(year, month, day))
                .find_map(at);
        }
        // A day more than the span asks for: the hour a clock change skips
        // is on no day's clock.
        let days = if self.weekday.is_some() { 9 } else { 3 };
        today
            .iter_days()
            .take(days)
            .filter(|day| self.weekday.is_none_or(|w| day.weekday() == w))
            .find_map(at)
    }
}

/// `s` without its leading spaces and whichever of `words` it starts with,
/// each at most once and in that order.
fn skip<'a>(s: &'a str, words: &[&str]) -> &'a str {
    words.iter().fold(s.trim_start(), |s, word| {
        s.strip_prefix(word).unwrap_or(s).trim_start()
    })
}

/// The letters `s` starts with, and the rest.
fn word_at(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    s.split_at(
        s.find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(s.len()),
    )
}

/// The time of day at the start of `s`: `3am`, `3:30 pm`, `15:00`.
fn clock_at(s: &str) -> Option<NaiveTime> {
    let hour: String = s.chars().take_while(char::is_ascii_digit).collect();
    if hour.is_empty() || hour.len() > 2 {
        return None;
    }
    let mut rest = &s[hour.len()..];
    let mut minute = 0;
    let on_the_minute = rest.starts_with(':');
    if let Some(m) = rest.strip_prefix(':') {
        let digits: String = m.chars().take_while(char::is_ascii_digit).collect();
        if digits.len() != 2 {
            return None;
        }
        minute = digits.parse().ok()?;
        rest = &m[2..];
    }
    let mut hour: u32 = hour.parse().ok()?;
    let rest = rest.trim_start().replace('.', "");
    if rest.starts_with("am") || rest.starts_with("pm") {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        if rest.starts_with("pm") {
            hour += 12;
        }
    } else if !on_the_minute {
        // `resets 3` alone is too thin to read as a time.
        return None;
    }
    NaiveTime::from_hms_opt(hour, minute, 0)
}

#[cfg(test)]
mod tests {
    use chrono::Timelike;

    use super::*;

    /// Tuesday noon, UTC.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap()
    }

    fn utc(month: u32, day: u32, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
        Utc.with_ymd_and_hms(2026, month, day, hour, minute, 0)
            .single()
    }

    fn claude(text: &str) -> Option<Limit> {
        limit_in("claude", text, now())
    }

    #[test]
    fn a_message_at_the_end_of_the_turn_is_a_limit() {
        let limit = claude("> fix it\n\n● You've hit your limit · resets 3am (UTC)\n").unwrap();
        assert_eq!(
            limit,
            Limit {
                hard: true,
                until: utc(9, 30, 3, 0),
                model_scoped: false,
                no_credit: false,
                line: "You've hit your limit · resets 3am (UTC)".into(),
            }
        );
        assert_eq!(limit.retry_at(now()), utc(9, 30, 3, 0).unwrap());
        // The older marker, a curly apostrophe and capitals read the same.
        for said in [
            "⏺ You've hit your limit",
            "● You’ve hit your limit",
            "● YOU'VE HIT YOUR LIMIT",
            "  ⎿  You've hit your limit",
            "You've hit your limit",
        ] {
            let limit = claude(&format!("> fix it\n{said}\n")).expect(said);
            assert!(limit.hard && !limit.model_scoped, "{said}");
            assert_eq!(limit.until, None, "{said}");
            assert_eq!(limit.line, said.trim_start_matches(['⏺', '●', '⎿', ' ']));
        }
    }

    /// A row's retry is the reset, else the wait `[limits]` sets for a
    /// limit that names none, else the one for no credit.
    #[test]
    fn the_retry_is_the_reset_else_the_wait_limits_sets() {
        let limits = LimitsConfig {
            unknown_reset_wait: "2h".into(),
            retry_after_no_credit: "1d".into(),
            ..Default::default()
        };
        let reset = claude("● You've hit your limit · resets 3am (UTC)\n").unwrap();
        assert_eq!(
            reset.retry_at_under(now(), &limits),
            utc(9, 30, 3, 0).unwrap()
        );
        let unknown = claude("● You've hit your limit\n").unwrap();
        assert!(!unknown.no_credit);
        assert_eq!(
            unknown.retry_at_under(now(), &limits),
            utc(9, 29, 14, 0).unwrap()
        );
        let broke = claude("● Credit balance is too low\n").unwrap();
        assert!(broke.no_credit);
        assert_eq!(
            broke.retry_at_under(now(), &limits),
            utc(9, 30, 12, 0).unwrap()
        );
    }

    /// What a waiting task says: the account (and model), the time it is
    /// tried again, what ran out and the task that saw it.
    #[test]
    fn a_row_says_what_ran_out_and_who_saw_it() {
        let limit = claude("● 5-hour limit reached ∙ resets 3am (UTC)\n").unwrap();
        assert_eq!(limit.what(), "5-hour limit");
        let row = AccountLimit::of(
            "claude-personal",
            Some("opus"),
            &limit,
            Some(412),
            Some("pi-1"),
            Some("claude-personal"),
            now(),
            &LimitsConfig::default(),
        );
        assert_eq!(row.model, None, "not one model's: the account's");
        assert!(row.stops(Some("opus")) && row.stops(None));
        let at = local_time(row.retry_at, now());
        assert_eq!(
            row.note(now()),
            format!("claude-personal exhausted until {at} (5-hour limit, seen by t-412)")
        );
        let opus = claude("● Opus weekly limit reached\n").unwrap();
        assert_eq!(opus.what(), "Opus weekly limit");
        let row = AccountLimit::of(
            "me",
            Some("opus"),
            &opus,
            None,
            None,
            None,
            now(),
            &LimitsConfig::default(),
        );
        assert_eq!(row.model.as_deref(), Some("opus"));
        assert!(row.stops(Some("opus")) && !row.stops(Some("sonnet")) && !row.stops(None));
        assert!(row.note(now()).starts_with("me opus exhausted until "));
        assert_eq!(
            claude("● Credit balance is too low\n").unwrap().what(),
            "no credit"
        );
        assert_eq!(
            claude("● You've hit your limit\n").unwrap().what(),
            "usage limit"
        );
    }

    /// A curly apostrophe is longer in bytes than the straight one the
    /// scope is read with; the words said still come out whole.
    #[test]
    fn what_ran_out_survives_words_and_apostrophes_before_it() {
        for (line, what) in [
            ("You’ve hit your 5-hour limit", "5-hour limit"),
            (
                "You’ve reached your Opus weekly limit · resets 7pm",
                "Opus weekly limit",
            ),
            ("You've hit your Sonnet limit · resets 3am", "Sonnet limit"),
            ("Opus weekly limit reached", "Opus weekly limit"),
        ] {
            assert_eq!(what_of(line, true, false), what, "{line}");
        }
    }

    #[test]
    fn a_limit_with_no_reset_is_tried_again_in_an_hour() {
        let limit = claude("● Credit balance is too low\n").unwrap();
        assert_eq!(limit.until, None);
        assert_eq!(limit.retry_at(now()), utc(9, 29, 13, 0).unwrap());
    }

    #[test]
    fn the_words_before_limit_say_whether_it_is_one_models() {
        for (said, model_scoped) in [
            ("You've hit your limit", false),
            ("You've hit your session limit", false),
            ("You've hit your weekly limit · resets 7pm", false),
            ("You've hit your Opus limit", true),
            ("You've hit your Opus weekly limit", true),
            ("You've reached your Fable limit. Run /usage-credits", true),
            ("You've reached your usage limit.", false),
            ("5-hour limit reached ∙ resets 3pm", false),
            ("Weekly limit reached", false),
            ("Opus weekly limit reached ∙ resets Oct 6, 9am", true),
            ("Sonnet 4.5 weekly limit reached", true),
            ("Claude AI usage limit reached|1759201200", false),
            ("Claude usage limit reached.", false),
        ] {
            let limit = claude(&format!("● {said}\n")).expect(said);
            assert!(limit.hard, "{said}");
            assert_eq!(limit.model_scoped, model_scoped, "{said}");
        }
    }

    #[test]
    fn a_429_or_529_that_ended_the_turn_is_a_short_limit() {
        for said in [
            r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error"}}"#,
            "API Error: 429 Too Many Requests",
        ] {
            let limit = claude(&format!("● {said}\n")).expect(said);
            assert!(!limit.hard && !limit.model_scoped, "{said}");
            assert_eq!(limit.line, said);
        }
        for said in [
            "API Error: 500 Internal server error",
            "API Error: 4290",
            "API Error: Connection lost mid-response.",
            "API Error: 529 Overloaded · Retrying in 12s… (attempt 4/10)",
        ] {
            assert_eq!(claude(&format!("● {said}\n")), None, "{said}");
        }
    }

    #[test]
    fn a_line_that_does_not_start_with_the_message_is_no_limit() {
        for said in [
            "● All good, merged #31",
            "● \"usage limit reached\" is in three files",
            "● The agent said: You've hit your limit",
            "● Weekly limit reached is what the banner says",
            "● You've hit your limit and the parser reads it",
            "● You've hit your limits",
            "● You've hit your one two three four limit",
            "● one two three four limit reached",
            "● Credit balance is too low for that",
            "● 12: limit reached",
            "● Approaching usage limit · resets 3pm",
            "  ⎿  /upgrade to increase your usage limit.",
            "● limit",
        ] {
            assert_eq!(claude(&format!("> fix it\n\n{said}\n")), None, "{said}");
        }
        // What follows the message on its line may be anything that is not
        // the next word of a sentence.
        for said in [
            "● Weekly limit reached, resets Mon 9am",
            "● Weekly limit reached. Resets Mon 9am",
            "● You've hit your limit (resets 3am)",
        ] {
            assert!(claude(&format!("> fix it\n\n{said}\n")).is_some(), "{said}");
        }
    }

    #[test]
    fn only_what_follows_the_last_prompt_is_read() {
        let limit = "● Weekly limit reached ∙ resets Mon 9am";
        assert!(claude(&format!("> fix it\n\n{limit}\n")).is_some());
        for prompt in ["> carry on", "❯ carry on", ">", "❯\u{a0}carry on"] {
            let pane = format!("> fix it\n\n{limit}\n\n{prompt}\n\n● Done.\n");
            assert_eq!(claude(&pane), None, "{prompt}");
            assert_eq!(claude(&format!("{limit}\n{prompt}\n")), None, "{prompt}");
        }
        // The input box, under its rule, is not a prompt that was sent,
        // whatever is typed in it.
        for typed in ["", " carry on"] {
            let pane = format!("> fix it\n\n{limit}\n\n────\n❯{typed}\n────\n  auto mode on\n");
            assert!(claude(&pane).is_some(), "{typed:?}");
        }
        // Neither is a line that only starts like one.
        let pane = format!("{limit}\n>> merged\n❯1\n  > quoted\n");
        assert!(claude(&pane).is_some());
    }

    #[test]
    fn only_the_last_message_is_read() {
        let limit = "● Weekly limit reached";
        for after in [
            "● The suite is green.",
            "● Bash(make check)\n  ⎿  ok",
            "⏺ Done.",
        ] {
            assert_eq!(claude(&format!("{limit}\n\n{after}\n")), None, "{after}");
        }
        // What Claude draws under its last message is not one.
        let pane = format!("{limit}\n\n✻ Cooked for 2m 51s\n\n────\n❯ \n────\n  auto mode on\n");
        assert!(claude(&pane).is_some());
    }

    #[test]
    fn only_the_last_lines_of_the_turn_are_read() {
        let limit = "● Weekly limit reached";
        let under = |n: usize| {
            let lines: Vec<String> = (0..n).map(|i| format!("  line {i}\n\n")).collect();
            format!("> fix it\n{limit}\n{}", lines.concat())
        };
        assert!(claude(&under(TURN_LINES - 1)).is_some());
        assert_eq!(claude(&under(TURN_LINES)), None);
    }

    #[test]
    fn the_output_of_a_tool_call_is_no_limit() {
        let said = "You've hit your limit · resets 3am";
        for pane in [
            format!("● Bash(cat limit.txt)\n  ⎿  {said}\n"),
            format!("● Bash(cat\n      limit.txt)\n  ⎿  {said}\n"),
            format!("● Bash(cat limit.txt)\n  ⎿  first\n     {said}\n"),
            format!("● Bash(cat limit.txt)\n  ⎿  first\n\n     {said}\n"),
            format!("● Agent(review)\n  ⎿  Bash(cat limit.txt)\n     ⎿  {said}\n"),
            format!("● Done:\n\n  {said}\n"),
        ] {
            assert_eq!(claude(&pane), None, "{pane}");
        }
        for pane in [
            format!("> fix it\n  ⎿  {said}\n"),
            format!("> fix it\n  on two lines\n  ⎿  {said}\n"),
            format!("● Bash(make check)\n  ⎿  ok\n\n  ⎿  {said}\n"),
        ] {
            assert!(claude(&pane).is_some(), "{pane}");
        }
    }

    #[test]
    fn the_banner_under_the_input_box_is_a_limit() {
        let said = "You've hit your limit · resets 3am (UTC)";
        let pane = format!("● Done.\n\n────\n❯ \n────\n  auto mode on\n  {said}\n");
        let limit = claude(&pane).unwrap();
        assert_eq!((limit.until, limit.line.as_str()), (utc(9, 30, 3, 0), said));
        // What is being typed in the box is not under it.
        let pane = format!("● Done.\n\n────\n❯ it said\n  {said}\n────\n  auto mode on\n");
        assert_eq!(claude(&pane), None);
    }

    #[test]
    fn a_reset_on_the_next_line_of_the_message_is_read() {
        let pane = "● You've hit your session limit\n  · resets 3am (UTC)\n";
        let limit = claude(pane).unwrap();
        assert_eq!(limit.until, utc(9, 30, 3, 0));
        assert_eq!(limit.line, "You've hit your session limit");
        let pane = "● You've hit your session limit\n\n  · resets 3am (UTC)\n";
        assert_eq!(claude(pane).unwrap().until, None);
    }

    #[test]
    fn only_claude_and_agy_are_read() {
        let pane = "● You've hit your limit · resets 3am\n";
        assert!(limit_in("claude", pane, now()).is_some());
        for kind in ["agy", "opencode", "codex", "claude-personal", ""] {
            assert_eq!(limit_in(kind, pane, now()), None, "{kind}");
        }
        let pane = "RESOURCE_EXHAUSTED (code 429): Individual quota reached.\n";
        assert!(limit_in("agy", pane, now()).is_some());
        for kind in ["claude", "opencode", "codex", ""] {
            assert_eq!(limit_in(kind, pane, now()), None, "{kind}");
        }
    }

    /// agy's pane: a sent prompt, `said` as the end of its turn, its input
    /// box and footer.
    fn agy_pane(said: &str) -> String {
        format!(
            "╭──────────╮\n│ > review │\n╰──────────╯\n\n  Reading the module.\n\n{said}\n\n\
             ╭──────────╮\n│ >        │\n╰──────────╯\n ~/src/repo  gemini-3-pro\n"
        )
    }

    #[test]
    fn agy_reads_a_429_or_resource_exhausted_that_ends_its_turn() {
        let agy = |said: &str| limit_in("agy", &agy_pane(said), now());
        let quota = agy("  RESOURCE_EXHAUSTED (code 429): Individual quota reached.").unwrap();
        assert!(quota.hard && !quota.model_scoped && !quota.no_credit);
        assert_eq!(quota.until, None);
        assert_eq!(quota.what(), "usage limit");
        let short = agy("  TOO_MANY_REQUESTS (code 429): Slow down. Resets in 1m.").unwrap();
        assert!(!short.hard);
        assert_eq!(short.until, Some(now() + chrono::Duration::minutes(1)));
        assert_eq!(short.what(), "rate limit");
        for said in [
            // Another status and code.
            "  UNAUTHENTICATED (code 401): Sign in again.",
            // agy still at it.
            "  RESOURCE_EXHAUSTED (code 429): Quota reached. Retrying in 5s.",
            // Quoted, not said.
            "  The log says RESOURCE_EXHAUSTED (code 429): Individual quota reached.",
            "  resource_exhausted (code 429): in lower case, as code has it.",
        ] {
            assert_eq!(agy(said), None, "{said}");
        }
    }

    #[test]
    fn agy_reads_only_what_follows_its_last_prompt() {
        let limit = "  RESOURCE_EXHAUSTED (code 429): Individual quota reached.";
        let pane = agy_pane(limit).replacen(
            "╭──────────╮\n│ >        │",
            "╭──────────╮\n│ > go on  │\n╰──────────╯\n\n╭──────────╮\n│ >        │",
            1,
        );
        assert_eq!(limit_in("agy", &pane, now()), None, "{pane}");
        // No box at all: the last paragraph after the last prompt.
        let pane = format!("> review\n\n{limit}\n");
        assert!(limit_in("agy", &pane, now()).is_some());
        let pane = format!("{limit}\n\n> go on\n");
        assert_eq!(limit_in("agy", &pane, now()), None);
    }

    #[test]
    fn agy_reads_a_limit_after_a_boxed_prompt_with_no_trailing_input_box() {
        let limit = "  RESOURCE_EXHAUSTED (code 429): Individual quota reached.";
        let pane = format!("╭──────────╮\n│ > review │\n╰──────────╯\n\n{limit}\n");
        assert!(limit_in("agy", &pane, now()).is_some(), "{pane}");
    }

    #[test]
    fn a_unix_time_is_the_reset() {
        let reset = Utc.timestamp_opt(1_800_000_000, 0).single();
        assert_eq!(reset_in("usage limit reached|1800000000", now()), reset);
        assert_eq!(reset_in("usage limit reached|1800000000.", now()), reset);
        // One that has passed is no reset.
        assert_eq!(reset_in("usage limit reached|1759201200", now()), None);
        assert_eq!(reset_in("usage limit reached|soon", now()), None);
    }

    #[test]
    fn a_time_of_day_is_the_next_one_in_the_zone_it_names() {
        for (said, want) in [
            ("resets 3am (UTC)", utc(9, 30, 3, 0)),
            ("resets 3pm (UTC)", utc(9, 29, 15, 0)),
            ("resets 12am (UTC)", utc(9, 30, 0, 0)),
            ("resets 12pm (UTC)", utc(9, 30, 12, 0)),
            ("resets at 15:30 (UTC)", utc(9, 29, 15, 30)),
            ("resets at 3:30 pm (UTC)", utc(9, 29, 15, 30)),
            ("resets 1:10am (UTC)", utc(9, 30, 1, 10)),
            ("Your limit will reset at 3pm (UTC).", utc(9, 29, 15, 0)),
            ("try again at 3:05 PM (UTC)", utc(9, 29, 15, 5)),
            // Summer time in Lisbon is an hour ahead of UTC, and New York
            // four behind.
            ("resets 3am (Europe/Lisbon)", utc(9, 30, 2, 0)),
            ("resets 1pm (Europe/Lisbon)", utc(9, 30, 12, 0)),
            ("resets 3am (America/New_York)", utc(9, 30, 7, 0)),
            ("resets 9am (America/New_York)", utc(9, 29, 13, 0)),
        ] {
            assert_eq!(reset_in(said, now()), want, "{said}");
        }
        for said in [
            "resets 3",
            "resets 13pm",
            "resets 0am",
            "resets 3:5",
            "resets 25:00",
        ] {
            assert_eq!(reset_in(said, now()), None, "{said}");
        }
    }

    #[test]
    fn a_time_of_day_with_no_zone_is_local() {
        for said in [
            "resets 3am",
            "resets 3am (no such zone)",
            "resets 3am (CEST)",
        ] {
            let at = reset_in(said, now()).expect(said);
            let local = at.with_timezone(&Local);
            assert_eq!((local.hour(), local.minute()), (3, 0), "{said}");
            assert!(at > now() && at - now() <= chrono::Duration::hours(25));
        }
    }

    #[test]
    fn a_weekday_is_the_next_one() {
        for (said, want) in [
            ("resets Mon 9am (UTC)", utc(10, 5, 9, 0)),
            ("resets Monday 9am (UTC)", utc(10, 5, 9, 0)),
            ("resets on Wed at 9:30am (UTC)", utc(9, 30, 9, 30)),
            ("resets Sun, 11pm (UTC)", utc(10, 4, 23, 0)),
            // Today, when the time is still to come; else in a week.
            ("resets Tue 1pm (UTC)", utc(9, 29, 13, 0)),
            ("resets Tue 9am (UTC)", utc(10, 6, 9, 0)),
            ("resets Tue 12pm (UTC)", utc(10, 6, 12, 0)),
            ("resets Fri (UTC)", utc(10, 2, 0, 0)),
            // Tuesday has not ended in New York when it has in Lisbon.
            ("resets Tue 11pm (America/New_York)", utc(9, 30, 3, 0)),
            ("resets Tue 12:30am (Europe/Lisbon)", utc(10, 5, 23, 30)),
        ] {
            assert_eq!(reset_in(said, now()), want, "{said}");
        }
    }

    #[test]
    fn a_date_is_this_year_or_the_next() {
        for (said, want) in [
            ("resets Oct 6, 9am (UTC)", utc(10, 6, 9, 0)),
            ("resets October 6 at 9am (UTC)", utc(10, 6, 9, 0)),
            ("resets Oct 6th, 9:15am (UTC)", utc(10, 6, 9, 15)),
            ("resets on Tue, Oct 6, 9am (UTC)", utc(10, 6, 9, 0)),
            ("resets Oct 6 (UTC)", utc(10, 6, 0, 0)),
            ("resets Sep 29, 1pm (UTC)", utc(9, 29, 13, 0)),
            ("resets Oct 6, 9am (Europe/Lisbon)", utc(10, 6, 8, 0)),
            // Winter time, from the last Sunday of October.
            ("resets Nov 2, 9am (Europe/Lisbon)", utc(11, 2, 9, 0)),
        ] {
            assert_eq!(reset_in(said, now()), want, "{said}");
        }
        assert_eq!(
            reset_in("resets Jan 3, 9am (UTC)", now()),
            Utc.with_ymd_and_hms(2027, 1, 3, 9, 0, 0).single()
        );
        assert_eq!(
            reset_in("resets Sep 29, 9am (UTC)", now()),
            Utc.with_ymd_and_hms(2027, 9, 29, 9, 0, 0).single()
        );
        for said in ["resets Oct", "resets Oct 32, 9am", "resets Feb 30 (UTC)"] {
            assert_eq!(reset_in(said, now()), None, "{said}");
        }
    }

    #[test]
    fn a_duration_counts_from_now() {
        let secs = |s: i64| Some(now() + chrono::Duration::seconds(s));
        for (said, want) in [
            ("try again in 2 hours 13 minutes.", secs(2 * 3600 + 13 * 60)),
            (
                "or try again in 4 days 3 hours",
                secs(4 * 86_400 + 3 * 3600),
            ),
            ("Please try again in 20s", secs(20)),
            ("Please try again in 20s.", secs(20)),
            ("try again in 1h 30m", secs(5400)),
            ("try again in 1 hour, 5 minutes and 10 seconds", secs(3910)),
            ("try again in 1 week", secs(604_800)),
            ("retry in 5 min", secs(300)),
            ("resets in 45 minutes", secs(2700)),
            ("Try Again In 2 Hours", secs(7200)),
        ] {
            assert_eq!(reset_in(said, now()), want, "{said}");
        }
        assert_eq!(
            reset_in("try again in 1.5s", now()),
            Some(now() + chrono::Duration::milliseconds(1500))
        );
        for said in [
            "try again in a while",
            "try again in 5",
            "try again in 5 parsecs",
            "try again in 900 weeks",
            "in 20s",
        ] {
            assert_eq!(reset_in(said, now()), None, "{said}");
        }
    }

    #[test]
    fn a_message_with_no_time_has_no_reset() {
        for said in [
            "You've hit your limit",
            "Credit balance is too low",
            "resets soon",
            "reset your password at the door",
            "",
        ] {
            assert_eq!(reset_in(said, now()), None, "{said}");
        }
    }

    #[test]
    fn the_hour_a_clock_change_skips_falls_on_the_next_day() {
        // Clocks in Lisbon go from 01:00 to 02:00 on 2027-03-28.
        let now = Utc.with_ymd_and_hms(2027, 3, 27, 12, 0, 0).unwrap();
        assert_eq!(
            reset_in("resets 1:30am (Europe/Lisbon)", now),
            Utc.with_ymd_and_hms(2027, 3, 29, 0, 30, 0).single()
        );
        assert_eq!(
            reset_in("resets Sun 1:30am (Europe/Lisbon)", now),
            Utc.with_ymd_and_hms(2027, 4, 4, 0, 30, 0).single()
        );
    }

    #[test]
    fn the_hour_a_clock_change_repeats_is_next_on_its_second_pass() {
        // Clocks in New York go from 02:00 EDT back to 01:00 EST on
        // 2026-11-01: 01:30 is 05:30 UTC and again 06:30 UTC.
        let before = Utc.with_ymd_and_hms(2026, 11, 1, 5, 0, 0).unwrap();
        let between = Utc.with_ymd_and_hms(2026, 11, 1, 6, 10, 0).unwrap();
        for (said, now, want) in [
            ("resets 1:30am (America/New_York)", before, (5, 30)),
            ("resets 1:30am (America/New_York)", between, (6, 30)),
            ("resets Sun 1:30am (America/New_York)", between, (6, 30)),
            ("resets Nov 1, 1:30am (America/New_York)", between, (6, 30)),
        ] {
            assert_eq!(
                reset_in(said, now),
                Utc.with_ymd_and_hms(2026, 11, 1, want.0, want.1, 0)
                    .single(),
                "{said} at {now}"
            );
        }
    }
}
