### Added

- `pastor machine add --agent-key FILE` also writes the machine's agent key
  to the head's `authorized_keys`, on its own marked line, locked to `pastor
  bridge --agent --machine NAME`; `--dry-run` shows what would be added.
  `pastor machine authorized-key --write` does the same for a machine that is
  already there, and writes nothing when the line is already as it would be.
- `pastor machine check` adds a `reach` row for an ssh machine when
  `head_address` is set: from the machine, a batch-mode ssh ping to the head.
  It fails naming the destination and ssh's words, with the command that adds
  the key.
