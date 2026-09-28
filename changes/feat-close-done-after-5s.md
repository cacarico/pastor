### Changed

- `close_done_after` defaults to `5s` instead of `15m`, so a done task frees
  its machine slot almost at once. Set `close_done_after = "15m"` to keep
  time to attach and read its last screen.
