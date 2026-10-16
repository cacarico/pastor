# Claude Startup Dialogs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A task blocked on any of Claude's one-time startup dialogs is answered by pastor, one dialog per settle window, so a fresh machine runs tasks end to end; a person or orchestrator answers whatever is left with `pastor task send --pick "<option>"`.

**Architecture:** The single trust marker and key pair in `[agents]` becomes a per-agent list of dialogs (marker, answer), with built-in rows for the `claude` kind. The machine actor's saved-trust pass reads a blocked task's pane once, finds the first matching dialog, presses its keys, and records (task, dialog) in a new store table so each is pressed once. A new `picker` module parses Claude's pickers (numbered or not) and turns an option's text into arrow keys plus Enter; the limit picker and `--pick` reuse it. Claude's fullscreen offer is switched off at agent start by an env default.

**Tech Stack:** Rust 2024, tokio, rusqlite, clap, serde; tests against `src/herdr/fake.rs`; `make check` is the gate.

**Spec:** `docs/superpowers/specs/2026-10-14-claude-dialogs-design.md`

## Global Constraints

- `make check` (fmt, clippy with warnings as errors, full suite) passes before every commit. Commits are signed; conventional prefix, plain subject, a body with the why, no trailers.
- Nothing in an existing pastor.toml changes meaning: `trust_keys`, `trust_marker`, `trust_marker = ""`, `trust_keys = []` keep their behaviour.
- The bypass-permissions warning is never a built-in dialog. Only startup blocks (`Blocked` with `prompt_pending`) are read for dialogs.
- Built-in wording, verbatim from Claude Code 2.1.295: markers `Yes, I trust this folder`, `Try the new fullscreen renderer?`, `Make auto mode your default permission mode?`, `Teach auto mode about your environment?`, `Switch your default effort to`; picks `Not now`, `No, keep`, `Don't show again`, `No, keep`.
- Env default for the `claude` kind: `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`, under the definition's own `env`.
- Schema bump: 17 to 18 (`SCHEMA_VERSION` in `src/store.rs`, the line in `AGENTS.md`).
- Vocabulary: dialog, picker, option, pick. Never "plugin" for any of this.
- Docs: the unit test that checks every command and flag named in `README.md`, `docs/manual.md` and `skills/**/*.md` exists must keep passing; every `toml` fence in them must parse as a pastor.toml.

## Review Focus

- A pane whose bottom shows a picker with the cursor on the option to pick: zero arrows, Enter only. Pinned in Task 1.
- The auto-mode default dialog draws its options without numbers and with `hideIndexes`; its `No, keep <mode>` must still be found by the `No, keep` prefix. Pinned in Task 1 and Task 5.
- A dialog that shows after trust but whose row the user removed (`pick = ""`) must leave the task for a person without pressing trust's keys again. Pinned in Task 5.
- A `pick` whose option is not in the picker on screen must press nothing and not claim the dialog, so a later pass with the right screen still answers. Pinned in Task 5.
- An old database with `trust_sent = 1` must not have its trust pressed again after the upgrade. Pinned in Task 3.

---

### Task 1: `picker` module, shared by the limit picker

**Files:**
- Create: `src/picker.rs`
- Modify: `src/lib.rs` (add `pub mod picker;` next to `pub mod limit;`)
- Modify: `src/limit.rs` (`LimitPicker::stop_keys`, `LimitPicker::listed`, `claude_picker`, `is_key_hint`, `picker_option`, `unboxed`, `is_box_edge`)

**Interfaces:**
- Produces:
  ```rust
  /// A picker at the bottom of a pane: options in order, the one under the cursor.
  pub struct Picker { pub options: Vec<String>, pub selected: usize, pub numbered: bool }
  pub struct Found { pub picker: Picker, /// cleaned lines above the picker, nearest last
                     pub above: Vec<String> }
  pub fn at_bottom(text: &str) -> Option<Found>;
  impl Picker {
      /// Arrows from the cursor to the first option containing `want` (case-insensitive), then Enter. None: no such option.
      pub fn keys_to(&self, want: &str) -> Option<Vec<String>>;
      /// `1. Yes, 2. No`
      pub fn listed(&self) -> String;
  }
  pub fn is_key_hint(line: &str) -> bool;
  ```

- [ ] **Step 1: Write the failing tests** in `src/picker.rs` (module skeleton with the tests only)

```rust
//! Claude's pickers: a run of options above `Enter to confirm · Esc to
//! cancel`, one under the cursor. Shared by the limit picker, the startup
//! dialogs and `task send --pick`.

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
        assert_eq!(f.above.last().map(String::as_str), Some("· Flicker-free output"));
    }

    #[test]
    fn an_unnumbered_picker_needs_its_key_hints() {
        let f = at_bottom(AUTO).unwrap();
        assert_eq!(f.picker.options, ["Yes, set auto mode as my default permission mode", "No, keep default"]);
        assert!(!f.picker.numbered);
        // Without the hints two plain lines with a cursor are a prompt box, not a picker.
        let no_hints = "\u{276f} Explain this codebase\n  and stop\n";
        assert!(at_bottom(no_hints).is_none());
    }

    #[test]
    fn keys_go_from_the_cursor_to_the_option() {
        let p = at_bottom(RENDERER).unwrap().picker;
        assert_eq!(p.keys_to("not now").unwrap(), ["Down", "Enter"]);
        assert_eq!(p.keys_to("Yes, try").unwrap(), ["Enter"]);
        assert_eq!(p.keys_to("upgrade"), None);
        let t = at_bottom(TRUST).unwrap().picker;
        assert_eq!(t.keys_to("Yes, I trust this folder").unwrap(), ["Down", "Enter"]);
        let auto = at_bottom(AUTO).unwrap().picker;
        assert_eq!(auto.keys_to("No, keep").unwrap(), ["Down", "Enter"]);
        let up = "❯ 1. A\n  2. B\n  3. C\n";
        let mut p = at_bottom(up).unwrap().picker;
        p.selected = 2;
        assert_eq!(p.keys_to("A").unwrap(), ["Up", "Up", "Enter"]);
    }

    #[test]
    fn a_list_in_a_message_is_not_a_picker() {
        assert!(at_bottom("● Options:\n  1. keep\n  2. drop\n").is_none(), "no cursor");
        assert!(at_bottom("❯ 1. only one\n").is_none(), "one option");
        assert!(at_bottom("❯ 1. a\n❯ 2. b\n").is_none(), "two cursors");
        assert_eq!(at_bottom("x\n❯ 1. a\n  2. b\n").unwrap().picker.listed(), "1. a, 2. b");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib picker::`
Expected: compile error, `at_bottom` not found.

- [ ] **Step 3: Implement the module**

Move `unboxed`, `is_box_edge`, `is_key_hint` and `picker_option` out of `src/limit.rs` into `src/picker.rs` (make `is_key_hint` `pub`, the others private), and change `picker_option` so the number is optional:

```rust
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
/// Options are numbered from 1, or, under key hints only, plain lines
/// (Claude hides the numbers on some dialogs). A list in a message has no
/// cursor, so it is not one.
pub fn at_bottom(text: &str) -> Option<Found> {
    let text = text.replace('\u{a0}', " ");
    let lines: Vec<&str> = text
        .lines()
        .map(unboxed)
        .filter(|l| !l.is_empty() && !is_rule(l) && !is_box_edge(l))
        .collect();
    let mut end = lines.len();
    while end > 0 && is_key_hint(lines[end - 1]) && lines.len() - end < 2 {
        end -= 1;
    }
    let hinted = end < lines.len();
    let mut options = Vec::new();
    let mut selected = None;
    let mut start = end;
    while start > 0 {
        let Some((cursor, number, option)) = picker_option(lines[start - 1], hinted) else {
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
    if options.len() < 2 {
        return None;
    }
    let numbered = options.iter().enumerate().all(|(i, (n, _))| *n == Some(i + 1));
    let unnumbered = options.iter().all(|(n, _)| n.is_none());
    if !numbered && !(unnumbered && hinted) {
        return None;
    }
    let selected = selected? - start;
    Some(Found {
        picker: Picker {
            options: options.into_iter().map(|(_, o)| o).collect(),
            selected,
            numbered,
        },
        above: lines[..start].iter().map(|l| l.to_string()).collect(),
    })
}

impl Picker {
    pub fn keys_to(&self, want: &str) -> Option<Vec<String>> {
        let want = want.to_ascii_lowercase();
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

    pub fn listed(&self) -> String {
        self.options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("{}. {o}", i + 1))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `❯ 1. Stop and wait` or `2. Upgrade your plan`, as (under the cursor,
/// number, text). With `plain`, a line with no number counts too, under a
/// cursor (`❯`, `>`, `›`) or indented at least as far as one.
fn picker_option(line: &str, plain: bool) -> Option<(bool, Option<usize>, &str)> {
    let (cursor, rest) = match line.strip_prefix(['❯', '>', '›']) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, line),
    };
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if digits > 0 && let Some(option) = rest[digits..].strip_prefix(". ") {
        let option = option.trim();
        return (!option.is_empty()).then(|| (cursor, rest[..digits].parse().ok(), option));
    }
    let option = rest.trim();
    (plain && !option.is_empty() && !is_key_hint(option)).then_some((cursor, None, option))
}
```

`is_rule` already exists in `limit.rs`; make it `pub(crate)` there or move it too, whichever keeps `limit.rs` compiling (it is also used by `claude`'s turn reading, so keep it in `limit.rs` as `pub(crate) fn is_rule` and import it).

- [ ] **Step 4: Point the limit picker at it**

In `src/limit.rs`: delete the moved functions; `LimitPicker::stop_keys` becomes

```rust
pub fn stop_keys(&self) -> Option<Vec<String>> {
    crate::picker::Picker { options: self.options.clone(), selected: self.selected, numbered: true }
        .keys_to(STOP_AND_WAIT)
}
```

`listed` delegates the same way. `claude_picker` becomes:

```rust
fn claude_picker(text: &str, now: DateTime<Utc>) -> Option<LimitPicker> {
    let found = crate::picker::at_bottom(text)?;
    if !found.picker.numbered {
        return None;
    }
    let above: Vec<&str> = found.above.iter().rev().take(4).map(String::as_str).collect();
    // ... the existing `message`, `ours`, `limit` logic unchanged, using `found.picker.options` ...
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib picker:: limit::`
Expected: all pass, including the existing limit picker tests.

- [ ] **Step 6: Commit**

```bash
git add src/picker.rs src/lib.rs src/limit.rs
git commit -m "refactor(limit): move picker parsing to its own module

The startup dialogs and task send --pick need the same reading of a
Claude picker as the limit picker, and some dialogs draw their options
without numbers, so the parser takes those too under key hints."
```

---

### Task 2: Dialog table and env default in `[agents]`

**Files:**
- Modify: `src/config/mod.rs` (`AgentDef`, `Agents`, constants near `CLAUDE_TRUST_KEYS`, validation in `PastorConfig::load` after the `trust_keys` check, `Agents::launch`, tests near `codex_and_agents_with_a_trust_marker_hold_their_prompt`)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum Answer { Pick(String), Keys(Vec<String>) }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Dialog { pub name: String, pub marker: Option<String>, pub answer: Answer, pub needs_trust: bool }
  impl Agents {
      /// The startup dialogs `agent` may show, trust first; empty for an agent with none.
      pub fn dialogs(&self, agent: &str) -> Vec<Dialog>;
  }
  pub const CLAUDE_CLASSIC_RENDERER_ENV: (&str, &str) = ("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN", "1");
  ```
  `[[agents.<name>.dialogs]]` rows deserialize into `DialogDef { name, marker, pick: Option<String>, keys: Option<Vec<String>> }` on `AgentDef::dialogs: Vec<DialogDef>`.

- [ ] **Step 1: Write the failing tests** (in the `tests` module of `src/config/mod.rs`)

```rust
#[test]
fn claude_has_built_in_dialogs_trust_first() {
    let cfg = PastorConfig::default();
    let d = cfg.agents.dialogs("claude");
    let names: Vec<&str> = d.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["trust", "fullscreen", "auto-mode-default", "auto-mode-setup", "effort-default"]);
    assert!(d[0].needs_trust && !d[1].needs_trust);
    assert_eq!(d[0].answer, Answer::Keys(vec!["Down".into(), "Enter".into()]));
    assert_eq!(d[0].marker.as_deref(), Some("Yes, I trust this folder"));
    assert_eq!(d[1].answer, Answer::Pick("Not now".into()));
    assert_eq!(d[2].marker.as_deref(), Some("Make auto mode your default permission mode?"));
    assert_eq!(d[2].answer, Answer::Pick("No, keep".into()));
    assert!(cfg.agents.dialogs("codex").is_empty());
    assert!(cfg.agents.dialogs("agy").is_empty());
}

#[test]
fn trust_settings_shape_the_trust_dialog() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pastor.toml");
    std::fs::write(&path, "[agents.claude]\ntrust_marker = \"\"\n[agents.mine]\nkind = \"claude\"\ntrust_keys = []\n").unwrap();
    let cfg = PastorConfig::load(&path).unwrap();
    let claude = cfg.agents.dialogs("claude");
    assert_eq!(claude[0].name, "trust");
    assert_eq!(claude[0].marker, None, "empty marker: pressed without reading");
    let mine = cfg.agents.dialogs("mine");
    assert_eq!(mine[0].name, "fullscreen", "no trust keys: no trust row, the rest stay");
}

#[test]
fn dialog_rows_replace_remove_and_add() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pastor.toml");
    std::fs::write(
        &path,
        "[[agents.claude.dialogs]]\nname = \"fullscreen\"\nmarker = \"fullscreen renderer\"\npick = \"Yes, try it\"\n\
         [[agents.claude.dialogs]]\nname = \"auto-mode-setup\"\nmarker = \"x\"\npick = \"\"\n\
         [[agents.claude.dialogs]]\nname = \"bypass\"\nmarker = \"Bypass Permissions mode\"\nkeys = [\"Down\", \"Enter\"]\n\
         [[agents.agy.dialogs]]\nname = \"welcome\"\nmarker = \"Welcome\"\nkeys = [\"Enter\"]\n",
    )
    .unwrap();
    let cfg = PastorConfig::load(&path).unwrap();
    let d = cfg.agents.dialogs("claude");
    let names: Vec<&str> = d.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["trust", "fullscreen", "auto-mode-default", "effort-default", "bypass"]);
    assert_eq!(d[1].marker.as_deref(), Some("fullscreen renderer"));
    assert_eq!(d[1].answer, Answer::Pick("Yes, try it".into()));
    assert_eq!(d[4].answer, Answer::Keys(vec!["Down".into(), "Enter".into()]));
    assert_eq!(cfg.agents.dialogs("agy").len(), 1);
}

#[test]
fn dialog_rows_are_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pastor.toml");
    for (body, want) in [
        ("[[agents.claude.dialogs]]\nname = \"trust\"\nmarker = \"m\"\npick = \"p\"\n", "use trust_keys and trust_marker"),
        ("[[agents.claude.dialogs]]\nname = \"x\"\nmarker = \"\"\npick = \"p\"\n", "marker must not be empty"),
        ("[[agents.claude.dialogs]]\nname = \"x\"\nmarker = \"m\"\npick = \"p\"\nkeys = [\"Enter\"]\n", "pick or keys, not both"),
        ("[[agents.claude.dialogs]]\nname = \"x\"\nmarker = \"m\"\nkeys = [\"\"]\n", "key name must not be empty"),
        ("[[agents.claude.dialogs]]\nname = \"\"\nmarker = \"m\"\npick = \"p\"\n", "name must not be empty"),
        ("[[agents.claude.dialogs]]\nname = \"x\"\nmarker = \"m\"\npick = \"p\"\n[[agents.claude.dialogs]]\nname = \"x\"\nmarker = \"m\"\npick = \"p\"\n", "named twice"),
    ] {
        std::fs::write(&path, body).unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains(want), "{body}: {err}");
    }
}

#[test]
fn claude_starts_on_the_classic_renderer_unless_env_says_otherwise() {
    let cfg = PastorConfig::default();
    let spec = DispatchSpec { agent: "claude".into(), ..Default::default() };
    let launch = cfg.agents.launch(&spec).unwrap();
    assert_eq!(launch.env.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN").map(String::as_str), Some("1"));
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pastor.toml");
    std::fs::write(&path, "[agents.claude]\nenv = { CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN = \"\" }\n[agents.mine]\nkind = \"claude\"\n").unwrap();
    let cfg = PastorConfig::load(&path).unwrap();
    assert_eq!(cfg.agents.launch(&spec).unwrap().env.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN").map(String::as_str), Some(""));
    let mine = DispatchSpec { agent: "mine".into(), ..Default::default() };
    assert_eq!(cfg.agents.launch(&mine).unwrap().env.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN").map(String::as_str), Some("1"), "the kind's default");
    let codex = DispatchSpec { agent: "codex".into(), ..Default::default() };
    assert!(cfg.agents.launch(&codex).unwrap().env.is_empty());
}
```

Check how the existing `launch` tests build a `DispatchSpec` (search `fn launch_` in the tests module) and build it the same way if `Default` is not enough.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::claude_has_built_in_dialogs_trust_first config::tests::dialog_rows`
Expected: compile errors on `dialogs`, `Answer`, `DialogDef`.

- [ ] **Step 3: Implement**

Next to `CLAUDE_TRUST_MARKER`:

```rust
/// Env every Claude task's pane gets, under the definition's own `env`:
/// the classic renderer. Claude 2.1's fullscreen renderer draws on the
/// alternate screen with no scrollback, which pastor's pane reads need,
/// and its startup offer ("Try the new fullscreen renderer?") is gated off
/// by this variable (checked in 2.1.295).
pub const CLAUDE_CLASSIC_RENDERER_ENV: (&str, &str) = ("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN", "1");

/// Claude's other startup dialogs (2.1.295 wording), each answered with
/// the option that keeps things as they are. The fullscreen row is a
/// fallback for a pane whose env lost `CLAUDE_CLASSIC_RENDERER_ENV`. The
/// bypass-permissions warning is not here: accepting it is a person's call.
const CLAUDE_DIALOGS: [(&str, &str, &str); 4] = [
    ("fullscreen", "Try the new fullscreen renderer?", "Not now"),
    ("auto-mode-default", "Make auto mode your default permission mode?", "No, keep"),
    ("auto-mode-setup", "Teach auto mode about your environment?", "Don't show again"),
    ("effort-default", "Switch your default effort to", "No, keep"),
];

/// How a dialog is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Arrow to the option whose text contains this, then Enter.
    Pick(String),
    /// These keys, as they are.
    Keys(Vec<String>),
}

/// A startup dialog an agent may show, and its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    pub name: String,
    /// Text only this dialog shows, in the pane's bottom prompt. `None`
    /// (trust with `trust_marker = ""`) matches any screen.
    pub marker: Option<String>,
    pub answer: Answer,
    /// Pressed only when the task's (machine, repo) is saved as trusted.
    pub needs_trust: bool,
}

/// A `[[agents.<name>.dialogs]]` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DialogDef {
    pub name: String,
    pub marker: String,
    /// The option to pick by its text. Empty with no `keys`: the dialog is
    /// removed (a built-in of this name is dropped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pick: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
}

impl DialogDef {
    /// `None` removes a dialog of this name.
    fn answer(&self) -> Option<Answer> {
        match (&self.pick, &self.keys) {
            (_, Some(keys)) if !keys.is_empty() => Some(Answer::Keys(keys.clone())),
            (Some(pick), _) if !pick.trim().is_empty() => Some(Answer::Pick(pick.clone())),
            _ => None,
        }
    }
}
```

Add to `AgentDef`:

```rust
    /// Startup dialogs beyond the trust one, `[[agents.<name>.dialogs]]`:
    /// a row named as a built-in replaces it; one with an empty `pick` and
    /// no `keys` removes it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dialogs: Vec<DialogDef>,
```

In `impl Agents`:

```rust
    /// The startup dialogs `agent` may show, in the order they are tried:
    /// the trust dialog (from `trust_keys` and `trust_marker`, absent when
    /// the agent has no trust keys), the built-ins of its kind as its own
    /// rows replace or remove them, then its other rows.
    pub fn dialogs(&self, agent: &str) -> Vec<Dialog> {
        let mut out = Vec::new();
        if let Some(keys) = self.trust_keys(agent) {
            out.push(Dialog {
                name: "trust".into(),
                marker: self.trust_marker(agent),
                answer: Answer::Keys(keys),
                needs_trust: true,
            });
        }
        let own = self.0.get(agent).map(|d| d.dialogs.as_slice()).unwrap_or(&[]);
        let builtin: &[(&str, &str, &str)] = if self.kind(agent) == "claude" { &CLAUDE_DIALOGS } else { &[] };
        for (name, marker, pick) in builtin {
            match own.iter().find(|d| d.name == *name) {
                Some(row) => {
                    if let Some(answer) = row.answer() {
                        out.push(Dialog { name: row.name.clone(), marker: Some(row.marker.clone()), answer, needs_trust: false });
                    }
                }
                None => out.push(Dialog {
                    name: (*name).into(),
                    marker: Some((*marker).into()),
                    answer: Answer::Pick((*pick).into()),
                    needs_trust: false,
                }),
            }
        }
        for row in own.iter().filter(|d| !builtin.iter().any(|(n, _, _)| *n == d.name)) {
            if let Some(answer) = row.answer() {
                out.push(Dialog { name: row.name.clone(), marker: Some(row.marker.clone()), answer, needs_trust: false });
            }
        }
        out
    }
```

Validation, after the `trust_keys` check in `load`:

```rust
            let mut seen = std::collections::HashSet::new();
            for d in &def.dialogs {
                if d.name.trim().is_empty() {
                    anyhow::bail!("{}: agents.{name}.dialogs: name must not be empty", path.display());
                }
                if d.name == "trust" {
                    anyhow::bail!("{}: agents.{name}.dialogs: use trust_keys and trust_marker for the trust dialog", path.display());
                }
                if !seen.insert(d.name.as_str()) {
                    anyhow::bail!("{}: agents.{name}.dialogs: {} is named twice", path.display(), d.name);
                }
                if d.marker.trim().is_empty() {
                    anyhow::bail!("{}: agents.{name}.dialogs.{}: marker must not be empty", path.display(), d.name);
                }
                if d.pick.as_deref().is_some_and(|p| !p.trim().is_empty()) && d.keys.as_ref().is_some_and(|k| !k.is_empty()) {
                    anyhow::bail!("{}: agents.{name}.dialogs.{}: pick or keys, not both", path.display(), d.name);
                }
                if d.keys.iter().flatten().any(|k| k.trim().is_empty()) {
                    anyhow::bail!("{}: agents.{name}.dialogs.{}: a key name must not be empty", path.display(), d.name);
                }
            }
```

In `Agents::launch`, before the opencode block:

```rust
        if self.kind(&spec.agent) == "claude" {
            let (key, value) = CLAUDE_CLASSIC_RENDERER_ENV;
            env.entry(key.into()).or_insert_with(|| value.into());
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib config::`
Expected: all pass. If an existing test asserts a Claude launch has an empty env, update it to expect the renderer variable and say why in the assertion message.

- [ ] **Step 5: Commit**

```bash
git add src/config/mod.rs
git commit -m "feat(config): a dialog table per agent, and Claude's classic renderer

The trust marker and keys become the first row of the startup dialogs an
agent may show, with built-in rows for Claude's current ones and
[[agents.<name>.dialogs]] to add or change them. Claude's pane gets
CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1: its fullscreen renderer has no
scrollback for pastor to read, and the variable also turns its startup
offer off."
```

---

### Task 3: Store: answered dialogs, schema 18

**Files:**
- Modify: `src/store.rs` (`SCHEMA_VERSION`, a `V18_TABLES` const, the fresh-database table list, the migration chain after `if v < 17`, new methods next to `claim_trust_sent`, tests)
- Modify: `AGENTS.md` line `pastor.db   tasks (schema 17)` to 18

**Interfaces:**
- Produces:
  ```rust
  impl Store {
      /// Record that `dialog` was answered for task `id` with `keys`. True only for the first call per (task, dialog).
      pub fn claim_answered(&self, id: i64, dialog: &str, keys: &[String]) -> anyhow::Result<bool>;
      /// Whether `dialog` was answered for task `id`.
      pub fn answered(&self, id: i64, dialog: &str) -> anyhow::Result<bool>;
  }
  ```

- [ ] **Step 1: Write the failing tests** (in the store tests module; find `fn trust_sent_is_claimed_once` or the nearest trust test for the setup style)

```rust
#[test]
fn a_dialog_is_claimed_once_per_task() {
    let store = Store::open_in_memory().unwrap();
    let t = store.insert_task(new_task()).unwrap();
    assert!(!store.answered(t.id, "fullscreen").unwrap());
    assert!(store.claim_answered(t.id, "fullscreen", &["Down".into(), "Enter".into()]).unwrap());
    assert!(!store.claim_answered(t.id, "fullscreen", &["Enter".into()]).unwrap());
    assert!(store.answered(t.id, "fullscreen").unwrap());
    assert!(!store.answered(t.id, "trust").unwrap());
    assert!(store.claim_answered(t.id, "trust", &["Enter".into()]).unwrap());
}

/// A database from schema 17 with trust_sent set: the upgrade records
/// the trust dialog as answered, so it is not pressed again.
#[test]
fn upgrade_from_17_keeps_trust_answered() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pastor.db");
    {
        let store = Store::open(&path).unwrap();
        let t = store.insert_task(new_task()).unwrap();
        store.claim_trust_sent(t.id, &["Down".into(), "Enter".into()]).unwrap();
        let conn = store.conn.lock().recover();
        conn.execute("DROP TABLE answered_dialogs", []).unwrap();
        conn.execute("UPDATE meta SET value = '17' WHERE key = 'schema_version'", []).unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert!(store.answered(1, "trust").unwrap());
}
```

Use whatever helper the store tests already have to make a task (`new_task()` or the equivalent); read the first trust test in that module and copy its setup.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store::tests::a_dialog_is_claimed_once_per_task store::tests::upgrade_from_17`
Expected: compile error, `claim_answered` not found.

- [ ] **Step 3: Implement**

```rust
const SCHEMA_VERSION: i64 = 18;

/// Schema 18: the startup dialogs pastor answered for each task, so a
/// dialog is pressed once per task across restarts. `trust` rows are
/// kept here and in `tasks.trust_sent`, which pull machines report.
const V18_TABLES: &str = "CREATE TABLE IF NOT EXISTS answered_dialogs (
        task_id INTEGER NOT NULL,
        dialog TEXT NOT NULL,
        keys TEXT NOT NULL,
        at TEXT NOT NULL,
        PRIMARY KEY (task_id, dialog)
     );";
```

Add `V18_TABLES` where a fresh database runs the other `V*_TABLES` batches (follow how `V16_TABLES` is executed on a new database), and in the migration chain:

```rust
                // The dialogs answered per task (`claim_answered`). A row
                // already past its trust prompt is recorded as such, so
                // the upgrade does not press it again.
                if v < 18 {
                    tx.execute_batch(V18_TABLES)?;
                    tx.execute(
                        "INSERT OR IGNORE INTO answered_dialogs (task_id, dialog, keys, at)
                         SELECT id, 'trust', COALESCE(trust_keys, '[]'), ?1 FROM tasks WHERE trust_sent = 1",
                        params![Utc::now().to_rfc3339()],
                    )?;
                }
```

Methods next to `claim_trust_sent`:

```rust
    pub fn claim_answered(&self, id: i64, dialog: &str, keys: &[String]) -> anyhow::Result<bool> {
        let keys = serde_json::to_string(keys)?;
        let at = Utc::now().to_rfc3339();
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
                "INSERT OR IGNORE INTO answered_dialogs (task_id, dialog, keys, at) VALUES (?1, ?2, ?3, ?4)",
                params![id, dialog, keys, at],
            )?;
            Ok(n == 1)
        })
    }

    pub fn answered(&self, id: i64, dialog: &str) -> anyhow::Result<bool> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            Ok(conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM answered_dialogs WHERE task_id = ?1 AND dialog = ?2)",
                params![id, dialog],
                |r| r.get(0),
            )?)
        })
    }
```

Check `prune` deletes a task's rows from the other per-task tables (`task_summaries`); add `answered_dialogs` to the same statement.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib store::`
Expected: all pass, including any schema-version pin test (update its number).

- [ ] **Step 5: Commit**

```bash
git add src/store.rs AGENTS.md
git commit -m "feat(store): record the startup dialogs answered per task

Schema 18 adds answered_dialogs so each dialog is pressed once per task
across restarts; rows past their trust prompt are carried over."
```

---

### Task 4: Fake herdr: a chain of startup dialogs

**Files:**
- Modify: `src/herdr/fake.rs` (`State` fields `trust_prompt`, `trust_screen`, `trust_answered`; `set_trust_prompt`, `set_trust_screen`; `agent.start`, `agent.prompt`, `pane.send_keys`, `agent.read`; tests at the end)

**Interfaces:**
- Produces:
  ```rust
  impl FakeHerdr {
      /// Agents started from now on stop at each screen in turn, `blocked`, until `pane.send_keys` sends exactly its keys. Replaces `set_trust_prompt` + `set_trust_screen` for a chain; those two still set a one-dialog chain.
      pub fn set_startup_dialogs(&self, dialogs: Vec<(String, Vec<String>)>);
  }
  ```

- [ ] **Step 1: Write the failing test** (fake tests module)

```rust
/// Two startup dialogs: the agent stays blocked through the first answer,
/// shows the second screen, and is idle after the second answer.
#[tokio::test]
async fn startup_dialogs_are_answered_in_turn() {
    let fake = FakeHerdr::new();
    fake.set_startup_dialogs(vec![
        ("first\n❯ 1. No\n  2. Yes\n".into(), vec!["Down".into(), "Enter".into()]),
        ("second\n❯ 1. Go\n  2. Not now\n".into(), vec!["Down".into(), "Enter".into()]),
    ]);
    let created = fake.workspace_create(None, "x", &Default::default()).await.unwrap();
    let pane = created.root_pane.pane_id;
    fake.agent_start("t-1", "claude", &pane, &[]).await.unwrap();
    assert_eq!(fake.agent_list().await.unwrap()[0].agent_status, AgentStatus::Blocked);
    assert!(fake.agent_read("t-1", 100).await.unwrap().starts_with("first"));
    fake.pane_send_keys(&pane, &["Down".into(), "Enter".into()]).await.unwrap();
    assert_eq!(fake.agent_list().await.unwrap()[0].agent_status, AgentStatus::Blocked);
    assert!(fake.agent_read("t-1", 100).await.unwrap().starts_with("second"));
    fake.pane_send_keys(&pane, &["Down".into(), "Enter".into()]).await.unwrap();
    assert_eq!(fake.agent_list().await.unwrap()[0].agent_status, AgentStatus::Idle);
    assert_eq!(fake.agent_read("t-1", 100).await.unwrap(), "fake output\n");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib herdr::fake::tests::startup_dialogs_are_answered_in_turn`
Expected: compile error, `set_startup_dialogs` not found.

- [ ] **Step 3: Implement**

Replace the `trust_prompt: Option<Vec<String>>` and `trust_screen: String` fields with:

```rust
    /// Startup dialogs agents started from now on stop at, in order: the
    /// screen `agent.read` shows and the keys that answer it (see
    /// `set_startup_dialogs`, `set_trust_prompt`). Empty: none.
    dialogs: Vec<(String, Vec<String>)>,
    /// pane id -> how many of `dialogs` it has answered.
    dialogs_done: HashMap<String, usize>,
```

Keep `trust_answered: HashMap<String, Instant>` for the redraw window. Then:

```rust
    pub fn set_startup_dialogs(&self, dialogs: Vec<(String, Vec<String>)>) {
        let mut s = self.state.lock().unwrap();
        s.dialogs = dialogs;
    }

    /// One startup dialog, Claude's folder-trust screen, answered by `keys`.
    pub fn set_trust_prompt(&self, keys: Option<Vec<String>>) {
        let mut s = self.state.lock().unwrap();
        s.dialogs = keys.map(|k| vec![(CLAUDE_TRUST_SCREEN.to_string(), k)]).unwrap_or_default();
    }

    /// The screen of the first startup dialog, instead of Claude's folder-trust one.
    pub fn set_trust_screen(&self, text: &str) {
        let mut s = self.state.lock().unwrap();
        if let Some(first) = s.dialogs.first_mut() {
            first.0 = text.into();
        }
    }
```

A helper on `State`:

```rust
impl State {
    /// The dialog `pane` is at, if it still has one.
    fn dialog_at(&self, pane: &str) -> Option<&(String, Vec<String>)> {
        let done = self.dialogs_done.get(pane).copied().unwrap_or(0);
        self.dialogs.get(done)
    }
}
```

Then update each use:
- `agent.start`: `if !s.dialogs.is_empty() && !s.trust_idle { change_status(..., Blocked) }`.
- `agent.prompt`: `at_idle_trust = s.trust_idle && s.dialog_at(&found.pane_id).is_some()`.
- `pane.send_keys`: `answered = matches!(&input, PaneInput::Keys(k) if s.dialog_at(&pane_id).is_some_and(|(_, keys)| keys == k)) && (status is Blocked || s.trust_idle)`. When answered: `*s.dialogs_done.entry(pane_id.clone()).or_default() += 1;` then, only if `s.dialog_at(&pane_id).is_none()`, set `trust_answered` and change the status to Idle and send the event; otherwise stay Blocked (no event: herdr sees one form replaced by another).
- `agent.read`: `at_trust = found.is_some_and(|a| s.dialog_at(&a.pane_id).is_some() && (a.agent_status == Blocked || s.trust_idle))`; the text is `s.dialog_at(pane).map(|(screen, _)| screen.as_str())` when at a dialog, else `pane_text` or `"fake output\n"`. Keep the `pane_text` override winning as it does now.
- `trust_answered.contains_key` checks that meant "already answered" become `dialog_at(..).is_none()`.

- [ ] **Step 4: Run the fake's tests and the machine tests that use trust**

Run: `cargo test --lib herdr::fake:: machine::tests::`
Expected: all pass unchanged, plus the new one.

- [ ] **Step 5: Commit**

```bash
git add src/herdr/fake.rs
git commit -m "test(fake): a chain of startup dialogs

Claude shows several one-time dialogs on a new machine, one after the
other; the fake can now stop an agent at each in turn."
```

---

### Task 5: The actor answers every known dialog, once each

**Files:**
- Modify: `src/machine.rs` (`MachineCommand::AutoTrust` and `pub async fn auto_trust` stay as the command's name; the actor's `async fn auto_trust` becomes `auto_answer`; `trust_answered` field renamed `dialog_answered`; `deliver_pending_prompt` and `deliver_held_prompts` use the new name; tests after `a_task_still_blocked_after_the_trust_keys_is_left_to_a_human`)
- Modify: `src/shepherd.rs` (comments naming `auto_trust` for the actor's pass)
- Modify: `MachineSettings.agents` doc comment: "the dialogs each agent may show at startup and how they are answered"

**Interfaces:**
- Consumes: `Agents::dialogs`, `Answer`, `Dialog` (Task 2); `picker::at_bottom`, `Picker::keys_to` (Task 1); `Store::claim_answered`, `Store::answered` (Task 3); `FakeHerdr::set_startup_dialogs` (Task 4).
- Produces: event `task.answered` with detail `{"dialog": name, "keys": [...]}`; `task.trusted` unchanged for the trust row.

- [ ] **Step 1: Write the failing tests**

```rust
    fn answered_events(events: &mut broadcast::Receiver<Event>) -> Vec<Event> {
        drain(events).into_iter().filter(|e| e.kind == "task.answered").collect()
    }
```

(copy the shape of `trusted_events` next to it.)

```rust
    const AUTO_SCREEN: &str = "Make auto mode your default permission mode?\n\n ❯ Yes, set auto mode as my default permission mode\n   No, keep default\n\n Enter to confirm · Esc to cancel\n";
    const RENDERER_SCREEN: &str = "  Try the new fullscreen renderer?\n\n  ❯ 1. Yes, try it\n    2. Not now\n\n  Enter to confirm · Esc to cancel\n";

    /// Trust, then the auto-mode nudge: each answered once, in order, and
    /// the prompt goes in after the second.
    #[tokio::test(start_paused = true)]
    async fn a_chain_of_startup_dialogs_is_answered_one_per_settle() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (CLAUDE_TRUST_SCREEN_FOR_TESTS.into(), vec!["Down".into(), "Enter".into()]),
            (AUTO_SCREEN.into(), vec!["Down".into(), "Enter".into()]),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the task to run", || state_of(&store, t.id) == TaskState::Running).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [
                PaneInput::Keys(vec!["Down".into(), "Enter".into()]),
                PaneInput::Keys(vec!["Down".into(), "Enter".into()]),
                PaneInput::Text("do it".into()), // or however the prompt shows in pane_input; copy from an existing test
            ]
        );
        assert_eq!(trusted_events(&mut events).len(), 1);
        let evs = answered_events(&mut events);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].detail, Some(serde_json::json!({"dialog": "auto-mode-default", "keys": ["Down", "Enter"]})));
        assert!(store.answered(t.id, "auto-mode-default").unwrap());
    }

    /// A dialog after trust with no row (the user removed it) is left for
    /// a person; trust's keys are not pressed again.
    #[tokio::test(start_paused = true)]
    async fn a_removed_dialog_is_left_for_a_person() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![
            (CLAUDE_TRUST_SCREEN_FOR_TESTS.into(), vec!["Down".into(), "Enter".into()]),
            (RENDERER_SCREEN.into(), vec!["Down".into(), "Enter".into()]),
        ]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.trust_repo("m", "/r").unwrap();
        let mut settings = settings();
        settings.agents = toml::from_str(
            "[claude]\n[[claude.dialogs]]\nname = \"fullscreen\"\nmarker = \"x\"\npick = \"\"\n",
        ).unwrap();
        let (h, mut events) = connected_with(&fake, &store, settings).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert_eq!(fake.pane_input(t.pane_id.as_deref().unwrap()), [PaneInput::Keys(vec!["Down".into(), "Enter".into()])]);
        assert!(answered_events(&mut events).is_empty());
    }

    /// A pick whose option is not on screen presses nothing and claims
    /// nothing, so a later screen with the option is still answered.
    #[tokio::test(start_paused = true)]
    async fn a_pick_not_in_the_picker_is_not_pressed() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(
            "  Try the new fullscreen renderer?\n\n  ❯ 1. Yes, try it\n    2. Later\n\n  Enter to confirm · Esc to cancel\n".into(),
            vec!["Down".into(), "Enter".into()],
        )]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
        assert!(calls(&fake, "pane.send_keys").is_empty());
        assert!(!store.answered(t.id, "fullscreen").unwrap());
    }

    /// An untrusted repo skips the trust row but not the rows after it:
    /// a task that starts on the renderer offer (trust already saved by
    /// Claude itself) is answered.
    #[tokio::test(start_paused = true)]
    async fn rows_after_trust_do_not_need_saved_trust() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(RENDERER_SCREEN.into(), vec!["Down".into(), "Enter".into()])]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        wait_for("the task to run", || state_of(&store, t.id) == TaskState::Running).await;
        assert!(trusted_events(&mut events).is_empty());
        assert_eq!(answered_events(&mut events).len(), 1);
    }

    /// Restarted actor, same store: an answered dialog is not pressed again.
    #[tokio::test(start_paused = true)]
    async fn an_answered_dialog_is_not_pressed_again_after_a_restart() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(RENDERER_SCREEN.into(), vec!["Enter".into()])]); // keys that do not answer it: stays blocked
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _e) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(calls(&fake, "pane.send_keys").len(), 1);
        h.shutdown().await;
        let (_h2, _e2) = connected(&fake, &store).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(calls(&fake, "pane.send_keys").len(), 1, "claimed in the store");
        assert_eq!(state_of(&store, t.id), TaskState::Blocked);
    }
```

Use the fake's `CLAUDE_TRUST_SCREEN` (make it `pub(crate)` in `fake.rs`) instead of `CLAUDE_TRUST_SCREEN_FOR_TESTS`. Find `connected` and whether a `connected_with(settings)` variant exists; if not, add one that takes `MachineSettings` and make `connected` call it with `settings()`. Check how an existing test shuts an actor down (`h.shutdown()` or dropping the handle) and how the prompt text appears in `pane_input`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib machine::tests::a_chain_of_startup_dialogs machine::tests::a_removed_dialog machine::tests::a_pick_not_in machine::tests::rows_after_trust machine::tests::an_answered_dialog`
Expected: `a_chain...` fails with the task still blocked after trust; `rows_after_trust` fails blocked; the rest fail on missing helpers or assertions.

- [ ] **Step 3: Implement `auto_answer`**

Rename the field `trust_answered` to `dialog_answered` everywhere in `machine.rs` (it already means "held after a startup answer"). Replace the actor's `auto_trust` body:

```rust
    /// Answer the startup dialog of every task blocked while starting
    /// (`Blocked` with its prompt pending): read its pane once, take the
    /// first of its agent's dialogs (`Agents::dialogs`) whose marker shows
    /// in the bottom prompt, skip it if this task was already answered for
    /// it or it needs a saved trust the task's repo lacks, press its keys,
    /// and record the answer (`claim_answered`, once per task and dialog
    /// across restarts). One dialog per pass: the next reconcile or status
    /// reads the pane again and finds what follows. The trust row also
    /// claims `trust_sent` and emits `task.trusted`, as before, so pull
    /// machines and hooks see no change; every other row emits
    /// `task.answered`. A send that fails claims nothing, so a later pass
    /// tries again. A `pick` not on screen presses nothing. A task no row
    /// matches, or still blocked after its rows, waits for a person.
    async fn auto_answer(&mut self) -> anyhow::Result<()> {
        for task in self.store.tasks_on_machine(&self.name)? {
            if task.state != TaskState::Blocked || !task.prompt_pending {
                continue;
            }
            let Some(pane) = &task.pane_id else { continue };
            let dialogs = self.settings.agents.dialogs(&task.spec.agent);
            if dialogs.is_empty() {
                continue;
            }
            let timeout = self.settings.request_timeout;
            let target = task.agent_name.as_deref().unwrap_or(pane);
            let screen = tokio::time::timeout(timeout, self.connector.agent_read(target, 100))
                .await
                .map_err(|_| TimedOut("agent.read", timeout))??;
            let Some(dialog) = dialogs.iter().find(|d| {
                d.marker.as_deref().is_none_or(|m| crate::config::shows_trust_marker(&screen, m))
            }) else {
                tracing::debug!(machine = %self.name, task = %task.display_id(), "blocked on a dialog pastor does not know; left for a person");
                continue;
            };
            if self.store.answered(task.id, &dialog.name)? {
                continue;
            }
            let repo = task.spec.repo.as_deref();
            if dialog.needs_trust
                && !repo.is_some_and(|r| self.store.is_trusted(&self.name, r).unwrap_or(false))
            {
                continue;
            }
            let keys = match &dialog.answer {
                crate::config::Answer::Keys(keys) => keys.clone(),
                crate::config::Answer::Pick(want) => {
                    let found = crate::picker::at_bottom(&screen).and_then(|f| f.picker.keys_to(want));
                    let Some(keys) = found else {
                        tracing::warn!(machine = %self.name, task = %task.display_id(), dialog = %dialog.name, pick = %want, "the dialog shows no such option; left for a person");
                        continue;
                    };
                    keys
                }
            };
            tokio::time::timeout(timeout, self.connector.pane_send_keys(pane, &keys))
                .await
                .map_err(|_| TimedOut("pane.send_keys", timeout))??;
            self.dialog_answered.insert(task.id, Instant::now());
            if !self.store.claim_answered(task.id, &dialog.name, &keys)? {
                continue;
            }
            if dialog.needs_trust {
                self.store.claim_trust_sent(task.id, &keys)?;
                tracing::info!(machine = %self.name, task = %task.display_id(), repo = repo.unwrap_or(""), "answered the trust prompt of a trusted repo");
                self.emit_with("task.trusted", Some(task.id), Some(serde_json::json!({"keys": keys})));
            } else {
                tracing::info!(machine = %self.name, task = %task.display_id(), dialog = %dialog.name, "answered a startup dialog");
                self.emit_with("task.answered", Some(task.id), Some(serde_json::json!({"dialog": dialog.name, "keys": keys})));
            }
        }
        Ok(())
    }
```

Note the behaviour change from today: the pane is read before the trust check, so an untrusted repo's task is read every pass (one `agent.read`, 100 lines, per blocked task per reconcile). That is the same read `confirm_pending_done` does for every idle task; acceptable.

`run_send` with `--trust` keeps calling `claim_trust_sent`; also call `claim_answered(task.id, "trust", &keys)` there so saved trust and `--trust` agree.

- [ ] **Step 4: Run the whole machine suite**

Run: `cargo test --lib machine::`
Expected: all pass. `saved_trust_leaves_a_dialog_that_is_not_the_trust_prompt_alone` uses the bypass screen, which no built-in matches, so it still passes; `a_task_still_blocked_after_the_trust_keys_is_left_to_a_human` still passes because the trust row is claimed and no other row matches the trust screen.

- [ ] **Step 5: Commit**

```bash
git add src/machine.rs src/shepherd.rs
git commit -m "feat(machine): answer every startup dialog pastor knows, once each

A task blocked while starting had only its trust prompt answered, and
once per task, so the dialog after it stranded the task on a new
machine. The pass now reads the pane, finds the first known dialog and
answers it, one per settle window, recording each so a restart presses
nothing twice."
```

---

### Task 6: `pastor task send --pick`

**Files:**
- Modify: `src/machine.rs` (`SendInput`: `pick` field, `is_empty`, `detail`; `run_send`)
- Modify: `src/task_cli.rs` (`SendArgs`, `send`)
- Modify: `src/daemon.rs` (`send`: the `nothing_to_send` message, the `--trust` exclusivity check)
- Tests: `src/machine.rs` tests, `src/daemon.rs` test near `nothing_to_send`

**Interfaces:**
- `SendInput { pick: Option<String> }`; errors `no_picker`, `no_such_option` as `SendRefused`.

- [ ] **Step 1: Write the failing tests** (machine tests)

```rust
    fn pick(what: &str) -> SendInput {
        SendInput { pick: Some(what.into()), ..Default::default() }
    }

    #[tokio::test(start_paused = true)]
    async fn send_pick_arrows_to_the_option_and_confirms() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![(
            "WARNING: Claude Code running in Bypass Permissions mode\n\n\u{276f} 1. No, exit\n  2. Yes, I accept\n\nEnter to confirm \u{b7} Esc to cancel\n".into(),
            vec!["Down".into(), "Enter".into()],
        )]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        h.send(t.id, pick("yes, i accept")).await.unwrap();
        assert_eq!(fake.pane_input(t.pane_id.as_deref().unwrap()), [PaneInput::Keys(vec!["Down".into(), "Enter".into()])]);
        let evs = input_events(&mut events);
        assert_eq!(evs[0].detail, Some(serde_json::json!({"keys": ["Down", "Enter"], "pick": "yes, i accept"})));
        assert!(!store.is_trusted("m", "/r").unwrap(), "--pick never saves trust");
    }

    #[tokio::test(start_paused = true)]
    async fn send_pick_refuses_without_a_picker_or_the_option() {
        let fake = FakeHerdr::new();
        fake.set_startup_dialogs(vec![("  Try the new fullscreen renderer?\n\n  ❯ 1. Yes, try it\n    2. Not now\n\n  Enter to confirm · Esc to cancel\n".into(), vec!["Enter".into()])]);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _e) = connected(&fake, &store).await;
        let t = h.dispatch(worktree_task(&store).id).await.unwrap();
        let err = h.send(t.id, pick("upgrade")).await.unwrap_err();
        let r = err.downcast_ref::<SendRefused>().unwrap();
        assert_eq!(r.code, "no_such_option");
        assert!(r.message.contains("1. Yes, try it, 2. Not now"), "{}", r.message);
        // Answered by hand: the pane shows no picker now.
        fake.pane_send_keys(t.pane_id.as_deref().unwrap(), &["Enter".into()]).await.unwrap();
        let err = h.send(t.id, pick("Not now")).await.unwrap_err();
        assert_eq!(err.downcast_ref::<SendRefused>().unwrap().code, "no_picker");
        assert!(calls(&fake, "pane.send_keys").len() == 1, "only the hand answer");
    }
```

Daemon test next to the existing `nothing_to_send` one:

```rust
    // --pick with text or --key is a usage error, like --trust.
    let resp = d.handle(IpcRequest::TaskSend { id, input: SendInput { pick: Some("No".into()), keys: vec!["esc".into()], ..Default::default() } }).await;
    assert_eq!(error_code(resp), "usage_error");
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib machine::tests::send_pick daemon::tests::`
Expected: compile error, no field `pick`.

- [ ] **Step 3: Implement**

`SendInput`:

```rust
    /// Answer the picker on screen by the text of one of its options:
    /// arrows to it, then Enter. Never with text, keys or trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<String>,
```

`is_empty`: `&& self.pick.is_none()`. `detail`: after the keys, `if let Some(p) = &self.pick { d.insert("pick".into(), p.clone().into()); }`.

`task_cli.rs`:

```rust
    /// Answer the picker on screen by the text of one of its options (case does not matter): the arrows to it, then Enter
    #[arg(long, value_name = "OPTION", group = "send_input", conflicts_with_all = ["text", "keys", "no_enter", "trust"])]
    pub pick: Option<String>,
```

and `pick: a.pick` in `send`. Update the doc comment `/// `pastor task send t-N [TEXT] [--key K]... [--no-enter] | --trust | --pick OPTION`.`

`daemon.rs` `send`: message `"give text, --key, --trust or --pick"`; add `if input.pick.is_some() && (input.text.is_some() || !input.keys.is_empty() || input.trust) { return IpcResponse::error("usage_error", "--pick answers the picker on screen; give no text, --key or --trust with it"); }`.

`run_send`, a new branch before the `else` that builds text and keys:

```rust
        } else if let Some(want) = &input.pick {
            let timeout = self.settings.request_timeout;
            let target = task.agent_name.as_deref().unwrap_or(&pane);
            let screen = match tokio::time::timeout(timeout, self.connector.agent_read(target, 100)).await {
                Ok(Ok(s)) => s,
                Ok(Err(err)) => { let dead = err.is_transport(); return (Err(err.into()), dead); }
                Err(_) => return (Err(TimedOut("agent.read", timeout).into()), true),
            };
            let Some(found) = crate::picker::at_bottom(&screen) else {
                let tail: Vec<&str> = screen.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect();
                let err = SendRefused { code: "no_picker", message: format!("{} shows no picker at the bottom of its pane:\n{}", task.display_id(), tail.join("\n")) };
                return (Err(err.into()), false);
            };
            let Some(keys) = found.picker.keys_to(want) else {
                let err = SendRefused { code: "no_such_option", message: format!("{} shows no option matching {want:?}; the picker has {}", task.display_id(), found.picker.listed()) };
                return (Err(err.into()), false);
            };
            let detail = serde_json::json!({"keys": keys, "pick": want});
            (None, keys, detail)
        } else {
```

- [ ] **Step 4: Run the suites**

Run: `cargo test --lib machine:: daemon:: && cargo test --test cli`
Expected: all pass. The CLI reference test (`src/cli_reference.rs`) regenerates its table from clap; run `cargo test --lib cli_reference::` and follow its message if it asks to update a fixture.

- [ ] **Step 5: Commit**

```bash
git add src/machine.rs src/task_cli.rs src/daemon.rs
git commit -m "feat(cli): task send --pick answers a picker by its option's text

Counting --key Down presses against a pane someone else read is error
prone for people and orchestrators alike; --pick reads the picker and
presses the arrows itself, and names the options when it cannot."
```

---

### Task 7: Docs, skill, changelog

**Files:**
- Modify: `docs/manual.md` (How it works: the trust paragraphs after "An agent started in a folder it has not seen"; the `task send` paragraph near "each `--key K` presses one named key"; Errors table rows `no_trust_keys`, `nothing_to_send`, add `no_picker`, `no_such_option`; Agent kinds table row **Trust**; Agent definitions section after "`env` values are passed as written"; Events: the `type` list and the `detail` list; the pastor.toml reference fence at `trust_marker = "Yes, I trust this folder"`)
- Modify: `README.md` near line 273 (one line: pastor answers Claude's startup dialogs by itself; the bypass warning and permission prompts stay yours)
- Modify: `skills/pastor/SKILL.md` (the `task send` example block: add `--pick`; the paragraph starting "Only starting, running and blocked tasks take input"; the troubleshooting bullet "`blocked` right after start")
- Modify: `AGENTS.md` (herdr facts: add a bullet; the schema line done in Task 3)
- Create: `changes/feat-claude-dialogs.md`

- [ ] **Step 1: Manual, How it works.** Replace the paragraph "From then on, when a task of a saved repo is blocked during startup..." through "...gets the keys without the check." with:

```markdown
From then on, a task blocked while it starts has its pane read at each
reconcile, and the first of its agent's startup dialogs whose marker shows
in the prompt at the bottom of the pane is answered: the head presses the
dialog's keys, once per task and dialog, and emits `task.answered` with the
dialog's name and the keys. The trust dialog is the first row; it is pressed
only for a saved repo, and emits `task.trusted` as before. Claude's are
built in, in its 2.1 wording, each answered with the option that keeps
things as they are: the fullscreen renderer offer (`Not now`), the auto-mode
default nudge (`No, keep ...`), the auto-mode setup offer (`Don't show
again`) and the default-effort switch (`No, keep ...`). Its
bypass-permissions warning is not: accepting it is yours to do, with
`pastor task send t-N --pick "Yes, I accept"`. One dialog is answered per
settle window, so a chain of them on a new machine takes a few windows. A
task blocked on a dialog no row matches, or still blocked after its rows,
waits for a person. The marker must be in the prompt at the bottom of the
pane (after the last output, and after any menu above the last one), since
the read includes scrollback where an earlier dialog can linger above a
later one. The trust row with no marker (`trust_marker = ""`) is pressed
without the check. `[[agents.<name>.dialogs]]` in pastor.toml adds a dialog
or changes a built-in (see [Agent definitions](#agent-definitions)).

The fullscreen offer rarely shows at all: every Claude task's pane gets
`CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`, which keeps Claude on its classic
renderer, the one with the scrollback pastor reads, and turns the offer
off. `env` in `[agents.<name>]` overrides it.
```

- [ ] **Step 2: Manual, `task send`.** After the sentence about `--key`, add: "`--pick OPTION` answers the picker on screen by the text of one of its options, case ignored: the head reads the pane, presses the arrows to the first option containing the text, then Enter, and records the keys and the text on `task.input`. It answers `no_picker` when the bottom of the pane shows none, and `no_such_option`, naming the options, when none matches. Like `--key`, it takes any task with a live pane and saves nothing."

- [ ] **Step 3: Manual, Errors table.** Add rows in alphabetical order:

```markdown
| `no_picker` | `task send --pick` to a task whose pane shows no picker at its bottom |
| `no_such_option` | `task send --pick` with text no option of the picker contains; the message lists them |
```

and change `nothing_to_send` to "`task send` with no text, `--key`, `--trust` or `--pick`".

- [ ] **Step 4: Manual, Agent definitions.** After the `env` paragraph add:

```markdown
`[[agents.<name>.dialogs]]` lists startup dialogs beyond the trust one,
one table per dialog: `name`, `marker` (text only that dialog shows) and
`pick` (the text of the option to choose) or `keys` (named keys, pressed as
they are). A row named as one of the kind's built-ins replaces it; a row
with `pick = ""` and no `keys` removes it. The trust dialog is set with
`trust_keys` and `trust_marker`, never here. Claude's built-ins are
`fullscreen`, `auto-mode-default`, `auto-mode-setup` and `effort-default`
(see [How it works](#how-it-works)).

```toml
# fragment of pastor.toml: accept Claude's bypass warning on this head's tasks
[[agents.claude.dialogs]]
name = "bypass"
marker = "Bypass Permissions mode"
pick = "Yes, I accept"
```
```

- [ ] **Step 5: Manual, reference fence and events.** In the pastor.toml fence after the `trust_marker` line add:

```
[[agents.claude.dialogs]]        # a startup dialog beyond trust; a built-in's name replaces it
name = "bypass"                  # its name, in task.answered
marker = "Bypass Permissions mode"  # text only that dialog shows
pick = "Yes, I accept"           # the option to choose by its text; or keys = ["Down", "Enter"]
```

In Events: add `task.answered` (the head answered one of the agent's startup dialogs) after `task.trusted` in the `type` list, and in `detail`: "on `task.answered`, `dialog` and `keys`; on `task.input`, also `pick` (sent by `--pick`)". Agent kinds table, Trust row for Claude: append "; its other startup dialogs are built in too (`task.answered`)".

- [ ] **Step 6: README, skill, AGENTS.md.** README after the permission-prompts bullet: "- **Claude's startup dialogs are answered for you.** Folder trust once a repo is saved, and the one-time offers Claude shows on a new machine; the bypass warning and permission prompts stay yours (`pastor task send --pick`)." SKILL.md example block: `pastor task send t-12 --pick "Not now"      # answer the picker on screen by its option's text`; in the paragraph after it, replace "a task blocked on any other dialog is left for you" with "Claude's other startup dialogs are answered too (`task.answered`); a task blocked on anything else is left for you: read the pane, then answer with `--pick` what a person would, never a permission prompt"; the troubleshooting bullet gains: "Any other dialog there (`pastor task read` shows it): `pastor task send t-N --pick \"<option>\"`." AGENTS.md herdr facts, after the folder-trust bullet: "- herdr's Claude manifest (`live_blocked_form`) reads every form with `Enter to confirm` and `Esc to cancel` under the last rule as `blocked`, so each of Claude's startup dialogs is a startup block to pastor, one after the other. `Actor::auto_answer` reads the pane and answers the first dialog it knows."

- [ ] **Step 7: Change file** `changes/feat-claude-dialogs.md`:

```markdown
### Added

- pastor answers Claude's other startup dialogs on its own, not only
  folder trust: the fullscreen renderer offer, the auto-mode nudges and
  the default-effort switch, each with the option that keeps things as
  they are, once per task and dialog, one per settle window
  (`task.answered`). `[[agents.<name>.dialogs]]` in pastor.toml adds a
  dialog or changes a built-in.
- `pastor task send --pick "<option>"` answers the picker on screen by the
  text of one of its options (`no_picker`, `no_such_option` otherwise).

### Changed

- Every Claude task's pane gets `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`,
  so Claude 2.1 stays on the classic renderer pastor can read and never
  offers the fullscreen one; `env` in `[agents.<name>]` overrides it.
- Schema 18: `answered_dialogs` records the dialogs answered per task.
```

- [ ] **Step 8: Run the docs tests and the gate**

Run: `make check`
Expected: passes, including the test that every command and flag in the docs exists and every `toml` fence parses.

- [ ] **Step 9: Commit**

```bash
git add docs/manual.md README.md skills/pastor/SKILL.md AGENTS.md changes/feat-claude-dialogs.md
git commit -m "docs: the startup dialogs pastor answers, and task send --pick"
```

---

### Task 8: Gate and pull request

- [ ] **Step 1:** `make check` on the whole branch; fix anything it finds in the task that owns the code.
- [ ] **Step 2:** `git push -u origin feat/claude-dialogs` and `gh pr create --base main --title "feat: answer every startup dialog Claude shows" --body-file -` with a body that names the spec and plan paths, the schema bump, the env default, and that `make smoke` on a fleet machine is still owed before trusting the dispatch change.
