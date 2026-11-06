### Fixed

- `pastor job enable|disable` no longer writes `enabled` inside a job
  file's multi-line string (e.g. a `description = """..."""` holding a
  blank line, an `enabled = ...` line or a `[table]` line); it edits the
  document with `toml_edit` and sets the root table's `enabled` key.
