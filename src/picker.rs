//! Claude's pickers: a run of options above `Enter to confirm · Esc to
//! cancel`, one under the cursor. Shared by the limit picker, the startup
//! dialogs and `task send --pick`.

/// A picker at the bottom of a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// The options as listed, without their numbers.
    pub options: Vec<String>,
    /// The option the cursor is on.
    pub selected: usize,
    /// Every option carries a number, from 1.
    pub numbered: bool,
}

/// What `at_bottom` found: the picker and the cleaned lines above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub picker: Picker,
    /// Non-empty lines above the picker, box sides and rules dropped, in
    /// order (the nearest last).
    pub above: Vec<String>,
}

/// The picker at the end of `text`, if one is there: a run of at least two
/// options, one under the cursor, under at most two lines of key hints.
/// Options are numbered from 1 (blank lines between them ignored), or,
/// under key hints only, the plain lines right above the hints up to the
/// blank line before them (Claude hides the numbers on some dialogs, and
/// its prose above an option list is set off by a blank line). A list in a
/// message has no cursor, so it is not one.
pub fn at_bottom(text: &str) -> Option<Found> {
    let text = text.replace('\u{a0}', " ");
    // Empty lines stay, as "", so a plain-option block has its bounds; a
    // rule or the edge of a box bounds it the same way.
    let lines: Vec<&str> = text
        .lines()
        .map(unboxed)
        .map(|l| {
            if crate::limit::is_rule(l) || is_box_edge(l) {
                ""
            } else {
                l
            }
        })
        .collect();
    let full: Vec<&str> = lines.iter().copied().filter(|l| !l.is_empty()).collect();
    let mut end = full.len();
    while end > 0 && is_key_hint(full[end - 1]) && full.len() - end < 2 {
        end -= 1;
    }
    let hinted = end < full.len();
    if let Some(found) = numbered(&full, end) {
        return Some(found);
    }
    if !hinted {
        return None;
    }
    // The block of plain lines right above the hints.
    let mut at = lines.len();
    while at > 0 && (lines[at - 1].is_empty() || is_key_hint(lines[at - 1])) {
        at -= 1;
    }
    let block_end = at;
    while at > 0 && !lines[at - 1].is_empty() && !is_key_hint(lines[at - 1]) {
        at -= 1;
    }
    let block = &lines[at..block_end];
    let mut selected = None;
    let mut options = Vec::new();
    for (i, line) in block.iter().enumerate() {
        let (cursor, option) = match line.strip_prefix(CURSORS) {
            Some(rest) => (true, rest.trim()),
            None => (false, line.trim()),
        };
        if cursor {
            if selected.is_some() {
                return None;
            }
            selected = Some(i);
        }
        options.push(option.to_string());
    }
    if options.len() < 2 {
        return None;
    }
    Some(Found {
        picker: Picker {
            options,
            selected: selected?,
            numbered: false,
        },
        above: lines[..at]
            .iter()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
    })
}

/// The cursor Claude (`❯`), Codex (`›`) or a plain terminal (`>`) draws.
const CURSORS: [char; 3] = ['❯', '>', '›'];

/// The numbered picker ending at `end` in `lines` (empty lines dropped).
fn numbered(lines: &[&str], end: usize) -> Option<Found> {
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
        options.push((number, option.to_string()));
        start -= 1;
    }
    options.reverse();
    let numbered = options.iter().enumerate().all(|(i, (n, _))| *n == i + 1);
    if options.len() < 2 || !numbered {
        return None;
    }
    let selected = selected? - start;
    Some(Found {
        picker: Picker {
            options: options.into_iter().map(|(_, o)| o).collect(),
            selected,
            numbered: true,
        },
        above: lines[..start].iter().map(|l| l.to_string()).collect(),
    })
}

impl Picker {
    /// Arrows from the cursor to the first option containing `want` (case
    /// ignored), then Enter. `None` when no option does, or `want` is
    /// blank (every option contains nothing); then nothing should be
    /// pressed.
    pub fn keys_to(&self, want: &str) -> Option<Vec<String>> {
        let want = want.trim().to_ascii_lowercase();
        if want.is_empty() {
            return None;
        }
        let at = self
            .options
            .iter()
            .position(|o| o.to_ascii_lowercase().contains(&want))?;
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

/// `line` without the sides of a box Claude draws around a dialog, and
/// without its indent.
fn unboxed(line: &str) -> &str {
    line.trim()
        .trim_start_matches('│')
        .trim_end_matches('│')
        .trim()
}

/// The top or bottom of a box: `╭────╮`.
pub fn is_box_edge(line: &str) -> bool {
    line.chars().count() >= 3
        && line
            .chars()
            .all(|c| matches!(c, '─' | '╭' | '╮' | '╰' | '╯' | '┌' | '┐' | '└' | '┘'))
}

/// A line of key hints under a picker: `Enter to confirm · Esc to cancel`.
pub fn is_key_hint(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ["to confirm", "to cancel", "to select", "to navigate"]
        .iter()
        .any(|h| lower.contains(h))
}

/// An option of a picker: `❯ 1. Stop and wait for limit to reset` or
/// `2. Upgrade your plan`, as (under the cursor, number, text).
fn picker_option(line: &str) -> Option<(bool, usize, &str)> {
    let (cursor, rest) = match line.strip_prefix(CURSORS) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, line),
    };
    let digits = rest.find(|c: char| !c.is_ascii_digit())?;
    let number = rest[..digits].parse().ok()?;
    let option = rest[digits..].strip_prefix(". ")?.trim();
    (!option.is_empty()).then_some((cursor, number, option))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RENDERER: &str = "──────\n  Try the new fullscreen renderer?\n\n  · Flicker-free output\n\n  ❯ 1. Yes, try it\n    2. Not now\n\n  Enter to confirm · Esc to cancel\n";
    const AUTO: &str = "Make auto mode your default permission mode?\n\n  Auto mode lets Claude handle permission prompts automatically.\n\n ❯ Yes, set auto mode as my default permission mode\n   No, keep default\n\n Enter to confirm · Esc to cancel\n";
    const TRUST: &str = "Quick safety check: Is this a project you created or one you trust?\n\n\u{276f} 1. No, exit\n  2. Yes, I trust this folder\n\nEnter to confirm \u{b7} Esc to cancel\n";

    #[test]
    fn a_numbered_picker_is_read_with_its_cursor() {
        let f = at_bottom(RENDERER).unwrap();
        assert_eq!(f.picker.options, ["Yes, try it", "Not now"]);
        assert_eq!(f.picker.selected, 0);
        assert!(f.picker.numbered);
        assert_eq!(
            f.above.last().map(String::as_str),
            Some("· Flicker-free output")
        );
    }

    #[test]
    fn an_unnumbered_picker_needs_its_key_hints() {
        let f = at_bottom(AUTO).unwrap();
        assert_eq!(
            f.picker.options,
            [
                "Yes, set auto mode as my default permission mode",
                "No, keep default"
            ]
        );
        assert!(!f.picker.numbered);
        // Without the hints two plain lines with a cursor are a prompt
        // box, not a picker.
        let no_hints = "\u{276f} Explain this codebase\n  and stop\n";
        assert!(at_bottom(no_hints).is_none());
    }

    /// A rule or box edge right above the options bounds the block as a
    /// blank line does: the prose over it is not an option.
    #[test]
    fn a_rule_above_plain_options_bounds_them() {
        let screen = "Make auto mode your default?\n\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} Yes\n  No, keep default\n\nEnter to confirm \u{b7} Esc to cancel\n";
        let f = at_bottom(screen).unwrap();
        assert_eq!(f.picker.options, ["Yes", "No, keep default"]);
        assert_eq!(f.above, ["Make auto mode your default?"]);
    }

    #[test]
    fn a_blank_pick_matches_nothing() {
        let p = at_bottom(RENDERER).unwrap().picker;
        assert_eq!(p.keys_to(""), None);
        assert_eq!(p.keys_to("  "), None);
    }

    #[test]
    fn keys_go_from_the_cursor_to_the_option() {
        let p = at_bottom(RENDERER).unwrap().picker;
        assert_eq!(p.keys_to("not now").unwrap(), ["Down", "Enter"]);
        assert_eq!(p.keys_to("Yes, try").unwrap(), ["Enter"]);
        assert_eq!(p.keys_to("upgrade"), None);
        let t = at_bottom(TRUST).unwrap().picker;
        assert_eq!(
            t.keys_to("Yes, I trust this folder").unwrap(),
            ["Down", "Enter"]
        );
        let auto = at_bottom(AUTO).unwrap().picker;
        assert_eq!(auto.keys_to("No, keep").unwrap(), ["Down", "Enter"]);
        let up = "❯ 1. A\n  2. B\n  3. C\n";
        let mut p = at_bottom(up).unwrap().picker;
        p.selected = 2;
        assert_eq!(p.keys_to("A").unwrap(), ["Up", "Up", "Enter"]);
    }

    #[test]
    fn a_list_in_a_message_is_not_a_picker() {
        assert!(
            at_bottom("● Options:\n  1. keep\n  2. drop\n").is_none(),
            "no cursor"
        );
        assert!(at_bottom("❯ 1. only one\n").is_none(), "one option");
        assert!(at_bottom("❯ 1. a\n❯ 2. b\n").is_none(), "two cursors");
        assert_eq!(
            at_bottom("x\n❯ 1. a\n  2. b\n").unwrap().picker.listed(),
            "1. a, 2. b"
        );
    }
}
