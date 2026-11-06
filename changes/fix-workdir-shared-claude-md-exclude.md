### Fixed

- A Claude workdir set whose block falls back to `.claude/CLAUDE.md` (the
  repo tracks `CLAUDE.local.md`) no longer hides a `.claude/CLAUDE.md` that was
  already there from `git status`. The file is kept out of git only when
  pastor created it, as for agy's `AGENTS.md`.
