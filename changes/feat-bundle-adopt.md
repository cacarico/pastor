### Added

- `pastor bundle adopt <machine> [--agent A] [--dry-run] [--json]` takes
  over the homes of a machine's agents, or one agent's home, in one step.
  It backs up every file it will touch first, under
  `~/.local/state/pastor/applied/backup/adopt-<time>/` on the machine, and
  writes nothing if that fails. It replaces a symlinked settings or
  instruction file, or a symlinked dir on an item's way, with a plain copy
  of what it points to, which is never written. It then applies every
  bundle, and the bundle's value wins where the user held another one. Each
  replacement is a `replaced` row with the old value and its backup path,
  and `--dry-run` lists the same rows and changes nothing. Everything pastor
  wrote goes in the ledger, so later applies keep the home as usual. The
  `bundles` row of `machine check` names adopt as the fix for `not adopted`
  and `conflict`.
- A harness manifest lists its login files under `paths.secrets`:
  Claude's `.credentials.json`, codex's `auth.json`, agy's
  `antigravity-oauth-token` and opencode's
  `~/.local/share/opencode/auth.json`. `bundle apply` and `bundle adopt`
  refuse an item at one with a `failed` row, and never read, copy or write
  it. Adopt does not copy a link or dir that holds one.
