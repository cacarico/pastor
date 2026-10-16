# Claude's startup dialogs: answer them all

Date: 2026-10-14. Branch `feat/claude-dialogs`.

## Why

A task on a fresh machine stops at Claude's own one-time dialogs. pastor
answers one of them, the folder-trust question, and only once per task. A
dialog that follows it, such as Claude 2.1's fullscreen renderer offer
("Try the new fullscreen renderer?") or the auto-mode nudges, leaves the
task `blocked` until a person attaches to the pane. That defeats what
pastor is for: adding a machine should be `machine add`, `trust add`, done.

Claude shows these dialogs as a numbered picker under a horizontal rule
with `Enter to confirm · Esc to cancel` below it. herdr's Claude manifest
(`live_blocked_form`) reads every such form `blocked`, so pastor already
sees each one as a startup block with the task's prompt still pending. What
is missing is a way to know the dialog and answer it.

## What changes

Four things, in the order a new machine meets them.

### 1. Switch off the fullscreen offer at agent start

Every Claude task's pane gets `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`.
Verified in Claude Code 2.1.295: the offer's gate returns false when the
alternate screen is disabled by env (`env_off`), so the dialog never shows.
The same variable keeps Claude on the classic renderer, which pastor needs:
the fullscreen renderer draws on the alternate screen with no scrollback,
and pastor reads the pane's last 100 lines to tell a question from a
finished turn.

The variable is a built-in of the `claude` kind, like its trust keys. It is
merged under the definition's own `env`, so `[agents.claude] env = {
CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN = "" }` (or any value) overrides it.
`task describe` shows the pane env as it does now.

### 2. A dialog table per agent

The single `trust_marker` and `trust_keys` pair becomes the first row of a
list of dialogs an agent may show at startup. A dialog has:

- `name`: a short id (`trust`, `fullscreen`), for events and config.
- `marker`: text only that dialog shows, matched in the bottom prompt of
  the pane as `shows_trust_marker` does today (case and whitespace
  ignored, scrollback above the last menu excluded).
- `pick`: the text of the option to choose, found wherever it sits in the
  picker (case-insensitive substring). The keys are arrows from the cursor
  to it, then Enter.
- `keys`: a fixed key sequence instead of `pick`, for a dialog that is not
  a picker (one of the two is set, never both).
- `needs_trust`: the row is pressed only when the task's (machine, repo) is
  saved as trusted. True for `trust` only.

Built-in rows of the `claude` kind, wording from the 2.1.295 bundle; each
picks the answer that keeps things as they are:

| name | marker | pick |
|------|--------|------|
| `trust` | `Yes, I trust this folder` | `Yes, I trust this folder` (needs saved trust) |
| `fullscreen` | `Try the new fullscreen renderer?` | `Not now` |
| `auto-mode-default` | `Make auto mode your default permission mode?` | `No, keep` |
| `auto-mode-setup` | `Teach auto mode about your environment?` | `Don't show again` |
| `effort-default` | `Switch your default effort to` | `No, keep` |

The bypass-permissions warning (`WARNING: Claude Code running in Bypass
Permissions mode`) is not built in: accepting it is a person's decision.
Permission prompts, Claude's questions and the limit picker are not
dialogs in this sense and keep their own handling.

The `trust` row keeps its old settings: `trust_keys` is its `keys` (built
in as `Down`, `Enter`, which `pick` would also produce) and `trust_marker`
its `marker`; `trust_marker = ""` still means press without reading, and
`trust_keys = []` still means the agent has no trust dialog. Nothing in an
existing pastor.toml changes meaning.

Config adds `[[agents.<name>.dialogs]]`, one table per row, with `name`,
`marker`, and `pick` or `keys`. A row whose `name` is a built-in's replaces
it; `pick = ""` with no `keys` (or `keys = []`) removes it. `needs_trust`
is not a config key: only the `trust` row has it, and `trust` cannot be
redefined this way, since `trust_keys` and `trust_marker` own it
(`agents.<name>.dialogs: use trust_keys and trust_marker for the trust
dialog` on load). Validation
mirrors `trust_keys`: no empty key names, a non-empty `marker`, `pick` and
`keys` not both set. Definitions of other kinds (`codex`, `agy`) have no
built-in rows; their `[[dialogs]]` are read the same way. Codex's trust
dialog, which herdr reads idle, keeps its `startup_markers` hold.

### 3. Answer once per dialog, not once per task

The reconcile pass that answers trust today (`auto_trust`, run after a
dispatch that blocked, on a blocked status event, and on every reconcile)
becomes `auto_answer`. For each task that is `blocked` with its prompt
pending and a pane:

1. Read the pane (`agent.read`, 100 lines) once.
2. Take the first dialog row whose marker shows in the bottom prompt. A
   `needs_trust` row whose repo is not saved as trusted is skipped, as is
   a task with no repo (the `trust add` path is unchanged). No match: the
   task is left for a person, as now.
3. Skip the row if this task has already been answered for it.
4. Press its keys. For `pick`, the picker is parsed from the pane
   (`limit.rs` option parsing, extended to options without numbers, which
   Claude draws for some dialogs: `❯ Yes, set auto mode ...` / `  No, keep
   default`). A `pick` whose option is not in the picker is not pressed;
   the task is left for a person and the reason is logged once.
5. Record (task, dialog, keys) in the store, so a restart does not press
   twice (`answered_dialogs` table, schema 18; `trust_sent` and
   `trust_keys` are kept and written for the `trust` row as today, so the
   pull-machine report path is unchanged).
6. Emit `task.answered` with `{dialog, keys}`. The `trust` row emits
   `task.trusted` as before instead, so existing hooks keep working.
7. Hold the task's prompt for `settle`, as after a trust answer today
   (`trust_answered`, renamed `dialog_answered`).

One row per pass. The next status event or reconcile reads the pane again
and finds the next dialog, so a chain (trust, then the auto-mode nudge) is
answered one dialog per settle window. A task blocked on something no row
matches, or still blocked after every matching row was pressed, waits for
a person, exactly as today.

Only startup blocks are answered. A task that goes `blocked` while running
(a permission prompt, a question) is never read for dialogs.

On a pull machine, the shepherd's actor runs the same pass with its own
pastor.toml and its copy of the head's trust table. Its `task.answered`
events are logged like its other events; `task.trusted` still reaches the
head through `TaskReport::trusted`.

### 4. `pastor task send t-N --pick "<option>"`

Answers any picker a person or orchestrator sees in a blocked task's pane
by the option's text instead of counting `--key Down`. The head reads the
pane, parses the picker at its bottom, presses the arrows and Enter, and
emits `task.input` with `keys` and `pick`. Errors, JSON on stderr like the
others:

- `no_picker`: the pane's bottom shows no picker (its last lines in the
  message).
- `no_such_option`: no option contains the text; the message lists the
  options as the picker numbers them.

`--pick` is input like `--key`: it takes any live task, not only one
blocked at startup, and never saves trust. It is refused with `--trust`,
`--key` or text (`one_input` style, reusing `nothing_to_send`'s shape).
The pastor skill tells orchestrators to read the pane and answer with
`--pick` what a person would, and never a permission prompt.

## Testing

Unit tests next to the code; the fake herdr gains a chain of startup
screens (`set_startup_dialogs`: each screen with the keys that clear it,
`set_trust_prompt` and `set_trust_screen` becoming a one-element chain).

- Config: `[[agents.x.dialogs]]` parses; a built-in is replaced by name
  and removed by an empty `pick`; `trust` in the list is refused; `pick`
  and `keys` together are refused; the env default is under the user's
  `env` and overridden by it.
- Picker parsing: numbered and unnumbered options, cursor anywhere, pick
  above and below the cursor, a missing option.
- Actor: trust then the auto-mode nudge answered in order, each once,
  prompt sent after the second; a dialog no row matches left alone; a
  `needs_trust` row skipped for an untrusted repo while a later row is
  not; an answer claimed once across an actor restart; `task.answered`
  and `task.trusted` emitted with their keys.
- `task send --pick`: the keys pressed on a numbered picker; `no_picker`
  and `no_such_option`; refused with other input.
- Docs: the existing test that every command and flag in the manual and
  skills exists covers the new flag and table.

`make check` passes before each commit. `make smoke` on a fleet machine
before trusting it, as `AGENTS.md` asks for dispatch changes.

## Docs

- `docs/manual.md`: How it works (the dialog table replaces the trust
  paragraph), pastor.toml reference (`[[agents.claude.dialogs]]`, the env
  default), events (`task.answered`, `pick` on `task.input`), error codes.
- `README.md`: one line under trust.
- `skills/pastor/SKILL.md`: `--pick`, the blocked-after-start note.
- `AGENTS.md`: the herdr fact that `live_blocked_form` reads every
  confirm/cancel form blocked, and the schema bump to 18.
- `changes/feat-claude-dialogs.md`.

## Out of scope

- Dialogs that need a person: login, the bypass warning, permission
  prompts, questions.
- Other Claude env defaults (telemetry, update checks, surveys): they
  change Claude's behaviour for the user, not pastor's reading of it.
- Reading dialogs on a task that is blocked mid-run.
