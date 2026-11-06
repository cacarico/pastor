### Changed

- A machine name starts with a letter or digit and holds only letters, digits,
  `.`, `_` and `-`, the rule the `authorized_keys` command and marker of a
  bridge key already needed. `pastor machine add` refuses a name that breaks it
  (`invalid_machine`). A flock.toml with such a name still loads, with a warning
  in the log naming the machine and the rule; rename it before it is used for a
  bridge key, which is refused.
