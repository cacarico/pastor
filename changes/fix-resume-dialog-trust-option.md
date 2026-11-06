### Fixed

- A resumed task that comes back blocked on a startup dialog pastor knows
  has it answered right away, as a dispatched task does, instead of at the
  next reconcile.
- `pastor task send --trust` no longer reads the trust option as selected
  in a menu without numbers when the cursor is on the option above it,
  such as "No, exit": each line of such a menu is its own option.
