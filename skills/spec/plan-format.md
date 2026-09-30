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

A plan whose tasks fan out uses `Order: waves` instead, with the wave list under it:

```markdown
- Order: waves.
  - Wave 1: Tasks 1, 2
  - Wave 1 merge: Task M1
  - Wave 2: Task 3
```

Wave N+1 starts only once wave N is merged: its merge task has written `Wave N: merged`, or, for a wave with one task, that task is `complete`; tasks in the same wave have no order between them and may run on different machines at once. A wave with one task is a series task in everything but name: it pushes the plan branch directly, with the prompt template from [Prompt file](#prompt-file), not the one for [a task in a wave](#a-task-in-a-multi-task-wave).

## Dispatch block

Last in each task. The fields first, then the exact command that starts the task. Only the command is run; the fields are for the human reading the plan.

```markdown
**Dispatch:**

- flock: `default`, tags: none, machine: any
- model: `sonnet`. Why: one file, the plan holds the code.
- timeout: `45m`
- depends on: Task 1
```

A task in a plan with `Order: waves` gives `wave: W` instead of `depends on:`, naming the wave it belongs to from the header's wave list:

```markdown
**Dispatch:**

- flock: `default`, tags: none, machine: any
- model: `sonnet`. Why: one file, the plan holds the code.
- timeout: `45m`
- wave: 1
```

followed by the command, in a `bash` block of its own:

```bash
pastor task run --prompt-file docs/superpowers/plans/YYYY-MM-DD-<name>/task-2.md \
  --flock default --repo '~/work/app' --worktree --branch pastor/<name>-2 \
  --model sonnet --timeout 45m --json
```

Rules for the command:

- `--prompt-file` is the task's prompt file, relative to the repo root; run it from a checkout of the plan branch.
- `--flock`, `--tag` (repeat it) or `--machine`, exactly as the fleet was read. Pin a machine only when the task needs something only that machine has.
- `--repo` is the path on the machine that runs the task, single-quoted when it starts with `~`.
- `--worktree --branch pastor/<name>-<N>`: a fresh local branch per task. The prompt resets it to the plan branch and pushes to the plan branch, so the local name never matters after the task. A task in a wave with more than one task uses `--branch pastor/<name>-task-<N>` instead, and pushes there, not the plan branch: sibling tasks in the same wave run at once, so none of them push the plan branch until a later merge task brings the wave's branches together. A wave with a single task keeps `pastor/<name>-<N>` and pushes the plan branch, exactly like a series task. The delimiter before `task` is a hyphen, not a slash: the plan branch `pastor/<name>` already exists as a ref, and Git refuses to create any ref under `refs/heads/pastor/<name>/...` while it does.
- `--model`: a `[models]` name from the model table. Leave `--agent` and `--agent-arg` out, so the task keeps the agent its flock or machine gives it, on the right account.
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

### A task in a multi-task wave

Same file naming and rules as above, but a task in a wave with more than one task pushes its own branch, not the plan branch, so there is nothing to rebase and nothing to retry: no sibling task can ever race it there.

```text
You are running unattended as task <N> of wave <W> in the plan <name>. Nobody will answer questions.

You are in a fresh git worktree of <repo description>. Run `git fetch origin` and `git reset --hard origin/pastor/<name>` first: that is the plan branch as wave <W> started, since no task in this wave pushes it before the wave is merged.

Read docs/superpowers/plans/<date>-<name>.md: the Global Constraints and Task <N> only. Do Task <N>'s steps in order, test first. Run `<check>` and keep it green; never commit while it fails. Commit as the steps say, with conventional commit messages whose body says why.

When the task is done: tick Task <N>'s boxes in the plan and commit that. Then append the line `Task <N>: complete (<first>..<last>, <check>: pass)` to docs/superpowers/plans/<date>-<name>.ledger.md, commit it, and `git push origin HEAD:pastor/<name>-task-<N>`.

If something is missing or a step cannot be done as written, do not guess past it: record `Task <N>: blocked: <why>` in the ledger the same way (append, commit, push), and stop.

Never push the plan branch or any other branch, never touch other worktrees, and do not open a pull request.

Print DONE as your last line.
```

### The merge task of a wave

A wave with two or more tasks ends with one more pastor task, its merge task, listed in the header as `Wave W merge: Task MW` and started only once every task of the wave has written its ledger line on its own branch. It merges the wave's branches into the plan branch in plan order, runs the check on the result and pushes the plan branch. The next wave starts from there.

It is mechanical on purpose. It checks, merges and runs the check; it never resolves a code conflict or fixes a failing check. Anything it cannot do as written becomes a `Wave W: blocked` ledger line, and a person decides. The task branches stay on the remote after the merge, until the plan's pull request merges.

Its prompt file is `task-M<W>.md` next to the others, and its Dispatch block names no wave task's branch:

```markdown
**Dispatch:**

- flock: `default`, tags: none, machine: any
- model: `sonnet`. Why: the merge is mechanical, and any judgment call becomes `blocked`.
- timeout: `30m`
- merges: wave 1 (Tasks 1, 2)
```

```bash
pastor task run --prompt-file docs/superpowers/plans/YYYY-MM-DD-<name>/task-M1.md \
  --flock default --repo '~/work/app' --worktree --branch pastor/<name>-m1 \
  --model sonnet --timeout 30m --json
```

The prompt, with `<list>` the wave's task numbers in plan order:

```text
You are running unattended as the merge task of wave <W> in the plan <name>. Nobody will answer questions. You merge; you do not write or fix code.

You are in a fresh git worktree of <repo description>. Run `git fetch origin` and `git reset --hard origin/pastor/<name>` first.

The wave's tasks are <list>. Each pushed its own branch, `origin/pastor/<name>-task-<N>`, and wrote its ledger line there. The ledger is docs/superpowers/plans/<date>-<name>.ledger.md.

To record a `blocked` line below, do this, in this order: nothing has been merged yet, or you have just reset back to `origin/pastor/<name>`, so there are no merge commits on HEAD to lose.

1. `git fetch origin` and `git rebase origin/pastor/<name>`, so the ledger you append to is the latest one.
2. Append the line to the ledger and commit it.
3. `git push origin HEAD:pastor/<name>`. If it is rejected as not a fast-forward, do steps 1 and 3 once more.

If that rebase stops on a conflict in the ledger, keep both sides' lines: the lines already on the plan branch first, yours after them. Then `git add` the ledger and `git rebase --continue`. If it stops on a conflict in any other file, or the second push is rejected too, run `git rebase --abort` if a rebase is in progress, push nothing, and print PUSH FAILED instead of DONE.

Then:

1. For each task <N> of the wave, in plan order, check that `origin/pastor/<name>-task-<N>` exists and that its ledger (`git show origin/pastor/<name>-task-<N>:docs/superpowers/plans/<date>-<name>.ledger.md`) has a `Task <N>: complete` line. If a branch is missing, or its ledger has no `complete` line for its task, record `Wave <W>: blocked: Task <N> <why>` and stop. Merge nothing.
2. For each task <N> in plan order, `git merge --no-ff origin/pastor/<name>-task-<N>`. If the merge stops on a conflict in the ledger only, keep both sides' lines, the plan branch's first and the task's after them, then `git add` the ledger and `git commit --no-edit`. If it stops on a conflict in any other file, run `git merge --abort`, go back with `git reset --hard origin/pastor/<name>`, record `Wave <W>: blocked: conflict in <paths> between Task <A> and Task <B>` (the task being merged and the earlier one that touched those paths) and stop. Do not resolve a conflict in any file but the ledger.
3. Run `<check>` on the merged result. If it fails, go back with `git reset --hard origin/pastor/<name>`, record `Wave <W>: blocked: <check> fails after merge` and stop. Do not fix it.
4. Append the line `Wave <W>: merged (Tasks <list>, <check>: pass)` to the ledger and commit it on top of the merge commits from step 2. Then `git push origin HEAD:pastor/<name>`. Do not rebase here, unlike a `blocked` line: HEAD now carries the wave's `--no-ff` merge commits, and `git rebase` drops merge commits and cherry-picks their contents instead, which would destroy them and likely conflict. If this push is rejected as not a fast-forward, stop and print PUSH FAILED instead of DONE: something else pushed the plan branch while this merge task ran, which should not happen, and a person needs to look before anything merges on top of it.

Never push the default branch or any task branch, never delete a branch, never touch other worktrees, and do not open a pull request.

Print DONE as your last line.
```

A merge task that stops on `blocked` has pushed nothing but its ledger line, so the plan branch is as the wave started it. Whoever resolves the conflict or the failing check merges by hand, appends `Wave W: merged (...)` and pushes, or runs the merge task again once a task branch is fixed.

## Ledger

A Markdown file with a short header, then one line per event, appended, never edited:

```text
Task 1: complete (a1b2c3d..e4f5a6b, make check: pass)
Task 1: ran as t-14 on pi-3
Task 2: blocked: the manual has no troubleshooting section
Task 2: ran as t-15 on pi-3
Task 3: complete (b7c8d9e..f0a1b2c, make check: pass)
Task 4: complete (c3d4e5f..a6b7c8d, make check: pass)
Task 3: ran as t-16 on pi-3
Task 4: ran as t-17 on pi-4
Wave 2: merged (Tasks 3, 4, make check: pass)
Task M2: ran as t-18 on pi-3
```

`Task 3` and `Task 4` ran at once in wave 2, each on its own branch; their lines reached the plan branch with the merge task's merges. A merge task writes `Wave W: merged (...)` or `Wave W: blocked: <why>`. With waves, the next task to run is the first task of the first wave with no `Wave W: merged` line; a wave with a single task has no merge task and counts as merged once its task is `complete`.

The agent writes `complete` and `blocked` lines, each after fetching and rebasing onto the plan branch, so a push that raced with another one is retried rather than lost.

Whoever starts a task keeps the id from `--json` and does not push the plan branch while that task's agent may still push it. Once the task has written its line, or its pane is closed, they append `Task N: ran as t-M on <machine>` and push, before starting the next task. That way `pastor task describe t-N` and `pastor events --task t-N` can be found from the branch alone. The next task to run is the first one with no `complete` line.
