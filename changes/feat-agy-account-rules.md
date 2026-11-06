### Added

- `pastor machine check` adds an `account-rules` row for each agy agent on
  a machine that has a permission profile. It warns, naming the rule and
  the allow, when a `deny` in agy's shared config
  (`~/.gemini/config/config.json`) covers something the profile allows or
  `pastor task done`: agy refuses the tool there, and pastor never writes
  that file.
