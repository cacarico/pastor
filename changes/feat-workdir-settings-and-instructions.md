### Added

- A workdir set's `settings` merge into the directory's
  `.claude/settings.local.json` key by key, as a home bundle's settings
  merge into a home, and its `instructions` become a marked block in
  `CLAUDE.local.md` (in `.claude/CLAUDE.md` when the repository tracks
  `CLAUDE.local.md`). What the agent or anyone else wrote in either file
  stays. A set may not name the permission keys a profile writes
  (`workdir_permissions_owned`), and a set that wants a file or key another
  set in the directory holds is refused, naming both (`workdir_conflict`).
