### Changed

- `close_done_after` defaults to `5s` instead of `15m`, so a done task frees
  its machine slot almost at once. Set `close_done_after = "15m"` to keep
  time to attach and read its last screen. A grace shorter than
  `reconcile_every` is checked on its own tick, so a done task closes about
  5s after it finishes, not at the next reconcile.
