You are running unattended as task 1 of the plan nested-herdr. Nobody will answer questions.

You are in a fresh git worktree of the pastor repository. Run `git fetch origin` and `git reset --hard origin/pastor/nested-herdr` first.

Read docs/superpowers/plans/2026-09-26-nested-herdr.md: the Global Constraints and Task 1 only. Do Task 1's steps in order, test first. Run `make check` and keep it green; never commit while it fails. Commit as the steps say, with conventional commit messages whose body says why.

When the task is done: tick Task 1's boxes in the plan and commit that. Then record it in the ledger and push, in this order:

1. `git fetch origin` and `git rebase origin/pastor/nested-herdr`, so the ledger you append to is the latest one.
2. Append the line `Task 1: complete (<first commit>..<last commit>, make check: pass)` to docs/superpowers/plans/2026-09-26-nested-herdr.ledger.md and commit it.
3. `git push origin HEAD:pastor/nested-herdr`. If it is rejected as not a fast-forward, do steps 1 and 3 once more.

If the rebase stops on a conflict in the ledger, keep both sides' lines: the ledger is append-only, so the lines already on the branch come first and yours after them. Then `git add` the ledger and `git rebase --continue`. If it stops on a conflict in any other file, or the second push is rejected too, run `git rebase --abort` if a rebase is in progress, push nothing, and print PUSH FAILED instead of DONE.

If something is missing or a step cannot be done as written, do not guess past it: record `Task 1: blocked: <why>` in the ledger the same way (fetch and rebase, append and commit, push), and stop.

Never push the default branch, never touch other branches or worktrees, and do not open a pull request.

Print DONE as your last line.
