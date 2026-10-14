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
