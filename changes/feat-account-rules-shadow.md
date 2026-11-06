### Added

- `pastor machine check` adds an `account-rules` row for each Claude agent
  on a machine that has a permission profile. It warns, naming the rule and
  the profile allow it shadows, when the account's own `settings.json` asks
  or denies something the profile allows: Claude applies an `ask` there over
  the allow, and a task, which never prompts, has the tool refused with
  nothing in its record of why. A warning does not
  fail the check, raise an event or change anything. The row has a new
  `warn` status in the report.
