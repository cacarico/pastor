### Fixed

- A workdir set that creates a shared file (`AGENTS.md`, `.agents/hooks.json`,
  `.claude/CLAUDE.md`) no longer lists it in the repo's exclude file while
  another worktree of the repo has an untracked copy of it, since that one
  line would hide the copy there. A `skipped` row names the worktree, and the
  next `place` looks again.
