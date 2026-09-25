You are running unattended as task 1 of the plan nested-herdr. Nobody will answer questions.

You are in a fresh git worktree of the pastor repository. Run `git fetch origin` and `git reset --hard origin/pastor/nested-herdr` first.

Read docs/superpowers/plans/2026-09-26-nested-herdr.md: the Global Constraints and Task 1 only. Do Task 1's steps in order, test first. Run `make check` and keep it green; never commit while it fails. Commit as the steps say, with conventional commit messages whose body says why.

When the task is done: tick Task 1's boxes in the plan, append the line `Task 1: complete (<first commit>..<last commit>, make check: pass)` to docs/superpowers/plans/2026-09-26-nested-herdr.ledger.md, and commit both.

If something is missing or a step cannot be done as written, do not guess past it: append `Task 1: blocked: <why>` to the ledger, commit, push, and stop.

Push with `git push origin HEAD:pastor/nested-herdr`. Never push the default branch, never touch other branches or worktrees, and do not open a pull request.

Print DONE as your last line.
