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

When the task is done: tick Task <N>'s boxes in the plan, append the line `Task <N>: complete (<first>..<last>, <check>: pass)` to docs/superpowers/plans/<date>-<name>.ledger.md, and commit both.

If something is missing or a step cannot be done as written, do not guess past it: append `Task <N>: blocked: <why>` to the ledger, commit, push, and stop.

Push with `git push origin HEAD:pastor/<name>`. Never push the default branch, never touch other branches or worktrees, and do not open a pull request.

Print DONE as your last line.
```

The prompt is not a template: `pastor task run` sends it verbatim, so there is no `{{ task.id }}`.

## Ledger

A Markdown file with a short header, then one line per event, appended, never edited:

```text
Task 1: dispatched t-14 on pi-3
Task 1: complete (a1b2c3d..e4f5a6b, make check: pass)
Task 2: dispatched t-15 on pi-3
Task 2: blocked: the manual has no troubleshooting section
```

The agent writes `complete` and `blocked` lines. Whoever starts a task writes the `dispatched` line with the id from `--json`, so `pastor task show t-N` and `pastor events --task t-N` can be found from the branch alone. The next task to run is the first one with no `complete` line.
