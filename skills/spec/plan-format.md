# Plan format for pastor

A pastor plan is a `superpowers:writing-plans` plan with two additions: a Pastor header after the plan's own header, and a Dispatch block at the end of every task. The steps, file lists and code stay as writing-plans writes them; the agent follows them.

## Pastor header

```markdown
## Pastor

- Plan branch: `pastor/<name>` (the plan, the ledger and the prompt files live here)
- Ledger: `docs/superpowers/plans/YYYY-MM-DD-<name>.ledger.md`
- Repo on the machines: `~/work/app`
- Check: `make check`
- Order: series. Start task N+1 only when the ledger has `Task N: complete`.
- Dispatch: do not push the plan branch while a task may push it. Record `Task N: ran as t-M on <machine>` after the task's own ledger line, before starting the next.
- Before merging: drop `docs/superpowers/plans/YYYY-MM-DD-<name>*` from the branch unless the repo keeps its plans.
```

## Dispatch block

Last in each task. The fields first, then the exact command that starts the task. Only the command is run; the fields are for the human reading the plan.

```markdown
**Dispatch:**

- flock: `default`, tags: none, machine: any
- model: `sonnet`. Why: one file, the plan holds the code.
- timeout: `45m`
- depends on: Task 1
```

followed by the command, in a `bash` block of its own:

```bash
pastor task run --prompt-file docs/superpowers/plans/YYYY-MM-DD-<name>/task-2.md \
  --flock default --repo '~/work/app' --worktree --branch pastor/<name>-2 \
  --agent claude --agent-arg --model --agent-arg sonnet --timeout 45m --json
```

Rules for the command:

- `--prompt-file` is the task's prompt file, relative to the repo root; run it from a checkout of the plan branch.
- `--flock`, `--tag` (repeat it) or `--machine`, exactly as the fleet was read. Pin a machine only when the task needs something only that machine has.
- `--repo` is the path on the machine that runs the task, single-quoted when it starts with `~`.
- `--worktree --branch pastor/<name>-<N>`: a fresh local branch per task. The prompt resets it to the plan branch and pushes to the plan branch, so the local name never matters after the task.
- `--agent` and `--agent-arg`: the agent and its model, from the model table.
- `--timeout` from the timeout guide. `--json` so whoever runs it reads the task id from the output.
- Keep it to plain words and single quotes: no `$`, no double quotes, no command substitution.

## Prompt file

One per task, `docs/superpowers/plans/YYYY-MM-DD-<name>/task-N.md`. Plain text; it is sent to the agent as is. Fill in the angle brackets:

```text
You are running unattended as task <N> of the plan <name>. Nobody will answer questions.

You are in a fresh git worktree of <repo description>. Run `git fetch origin` and `git reset --hard origin/pastor/<name>` first.

Read docs/superpowers/plans/<date>-<name>.md: the Global Constraints and Task <N> only. Do Task <N>'s steps in order, test first. Run `<check>` and keep it green; never commit while it fails. Commit as the steps say, with conventional commit messages whose body says why.

When the task is done: tick Task <N>'s boxes in the plan and commit that. Then record it in the ledger and push, in this order:

1. `git fetch origin` and `git rebase origin/pastor/<name>`, so the ledger you append to is the latest one.
2. Append the line `Task <N>: complete (<first>..<last>, <check>: pass)` to docs/superpowers/plans/<date>-<name>.ledger.md and commit it.
3. `git push origin HEAD:pastor/<name>`. If it is rejected as not a fast-forward, do steps 1 and 3 once more.

If the rebase stops on a conflict in the ledger, keep both sides' lines: the ledger is append-only, so the lines already on the branch come first and yours after them. Then `git add` the ledger and `git rebase --continue`. If it stops on a conflict in any other file, or the second push is rejected too, run `git rebase --abort` if a rebase is in progress, push nothing, and print PUSH FAILED instead of DONE.

If something is missing or a step cannot be done as written, do not guess past it: record `Task <N>: blocked: <why>` in the ledger the same way (fetch and rebase, append and commit, push), and stop.

Never push the default branch, never touch other branches or worktrees, and do not open a pull request.

Print DONE as your last line.
```

The prompt is not a template: `pastor task run` sends it verbatim, so there is no `{{ task.id }}`.

## Ledger

A Markdown file with a short header, then one line per event, appended, never edited:

```text
Task 1: complete (a1b2c3d..e4f5a6b, make check: pass)
Task 1: ran as t-14 on pi-3
Task 2: blocked: the manual has no troubleshooting section
Task 2: ran as t-15 on pi-3
```

The agent writes `complete` and `blocked` lines, each after fetching and rebasing onto the plan branch, so a push that raced with another one is retried rather than lost.

Whoever starts a task keeps the id from `--json` and does not push the plan branch while that task's agent may still push it. Once the task has written its line, or its pane is closed, they append `Task N: ran as t-M on <machine>` and push, before starting the next task. That way `pastor task show t-N` and `pastor events --task t-N` can be found from the branch alone. The next task to run is the first one with no `complete` line.
