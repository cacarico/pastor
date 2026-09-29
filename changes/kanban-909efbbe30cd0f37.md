### Changed

- Every request to a head is checked against its IPC protocol as it is
  sent, not per CLI command: a request carrying something the head predates
  is refused `head_too_old` before it goes out, with one ping per head. A
  headless serve's claims, reports, event reads and job submits now get the
  same check, so a pull machine or shepherd refuses a head too old for them
  instead of sending a request it would drop or refuse.
