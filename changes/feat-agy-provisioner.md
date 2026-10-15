### Added

- The agy provisioner: before each launch of an agy agent under a
  permission profile, pastor appends the deny rules of the machine's own
  profile (its `[[machine]]` profile, else its flock's, else `[defaults]`)
  to `permissions.deny` in agy's `~/.gemini/antigravity-cli/settings.json`
  on that machine. Additive and idempotent: no rule is removed or moved, no
  other key changes, a file that already covers every rule is not written,
  and the write is a temp file renamed into place only if agy has not saved
  the file meanwhile. A machine whose settings pastor cannot read or write
  (a `command` machine, a file that is not JSON, a symlink) fails the task
  with `agy_permissions_missing` and the rules to add by hand.
