---
name: review
description: "Takes a finished pastor:spec plan branch, runs the repo's own check locally, reviews the diff fresh, sends back one fix task for Critical and Important findings (dispatch's own rules, one model tier up) with one scoped re-review, defers Minor findings to the ledger, and opens the pull request, as a draft listing the findings if that re-review still finds a Critical one. Merges only when asked. Disabled for automatic invocation: it starts an agent on the flock and opens a pull request, so a person picks when."
disable-model-invocation: true
model: opus
effort: xhigh
argument-hint: "[branch or t-N]"
---

# pastor:review

Takes a `pastor:spec` plan branch from `complete` to a pull request: the
repo's own check, a fresh review, one fix task for what the review finds
and nothing beyond it, then the PR. It merges only when asked.

**REQUIRED BACKGROUND:** [plan-format.md](../spec/plan-format.md) for the
plan header, the Dispatch block and the ledger; the `pastor` skill (or
`pastor --skill`) for the CLI; `pastor:dispatch`'s own checklist for how a
task is started and watched, reused here for the fix task.

**Builds on:** a code-review skill for the review itself (any will do).

## Checklist

1. **Find the branch.** From the argument, a branch name. For a `t-N`,
   `pastor task describe t-N --json`: its `repo`, and its `prompt`, whose
   `git reset --hard origin/pastor/<name>` line names the plan branch and
   whose `Read docs/plans/<date>-<name>.md` line names the
   plan. The task's own branch (the Dispatch command's `--branch`,
   `pastor/<name>-<n>`) is a local branch on the machine that ran it and
   was never pushed; do not fetch it. `git fetch origin` the plan branch
   and check it out in a local worktree of this repo.
2. **Check it.** Run the plan header's `Check:` command here, in the local
   worktree, never on the flock. Read the plan's ledger for a `blocked:`
   line (a task that stopped on a question the plan never answered) and
   any `deferred` line a previous review pass already left — this pass
   does not relitigate what the last one decided.
3. **Review it fresh.** Hand the diff since the plan branch's base to
   the code-review skill, with the plan's Review Focus
   verbatim as what to look for. Use `opus`, or `fable` once the diff is
   over about 1,500 lines.
4. **One fix task, at most.**
   - Critical and Important findings go back as **one** fix task, never
     more, and it is one more task of the plan, so that `pastor:dispatch`
     runs it unchanged: dispatch's `next-task.sh` knows only the plan's
     numbered `### Task <n>:` headings and their Dispatch blocks, and the
     prompt template sends the agent to the plan for its steps. With `<n>`
     the plan's last task, append `### Task <n+1>: Fix the review
     findings` to the plan, its steps every Critical and Important finding
     verbatim — file, line, what is wrong — then the check and a commit,
     and a Dispatch block whose command is the other tasks' with
     `--prompt-file docs/plans/<date>-<name>/task-<n+1>.md`
     and `--branch pastor/<name>-<n+1>`. Write that prompt file from the
     [prompt file](../spec/plan-format.md#prompt-file) template, so it
     resets to the plan branch, reads Task `<n+1>`, and ends with `Task
     <n+1>: complete` in the ledger and a push. Give it a model one tier
     above whatever the plan used for the task that wrote the finding
     (`skills/spec/SKILL.md`'s model table: `sonnet` → `opus` → `fable`).
     Commit the plan and the prompt file, nothing else, and push the plan
     branch: the plan is `COMPLETE`, so no task can push it now. Then run
     `pastor:dispatch` on the plan: `next-task.sh` finds `RUN <n+1>`, and
     dispatch's own checklist starts and watches it (its step 2's `RUN`
     handling, step 3's wait, step 4's rules) through to `COMPLETE`.
   - Minor findings never get a fix task: append one ledger line per
     finding, `Task <n>: deferred (Minor): <finding>`, the same
     fetch-rebase-append-commit-push-retry-once dance any ledger line
     uses, in the same commit as the fix task when there is one, so the
     plan branch is pushed once, before the fix task starts.
   - Once dispatch's done rule has confirmed the fix task (its `Task
     <n+1>: complete` line is on the plan branch), review only the diff it
     added, against the same findings it was asked to fix — a scoped
     re-review, not a fresh one.
   - If that scoped re-review still finds a Critical finding, send no
     second fix task. Go on to step 5 with the PR as a draft and those
     still-open findings in its body; there is no step 6 after that until
     the user acts.
5. **Open the PR.** Drop `docs/plans/<date>-<name>*` from the
   branch first, unless the plan header's "Before merging" line says the
   repo keeps its plans: one commit on the plan branch, pushed from here —
   the plan is `COMPLETE` again, so no task can push it, and a PR opened
   before that push would carry the plan files in its diff. Then `gh pr
   create`: the body comes from the plan header, what the check and the
   review ruled, and any deferred or still-open findings; a draft PR only
   in step 4's last case. Ask for no reviewers — the repo's own gate does
   that — and merge only when the user asks.
6. **Close the plan's tasks.** `pastor task close t-N --remove-worktree`
   for every `t-<id>` the ledger's `ran as` lines name, the fix task
   included. If step 5 dropped the plan files, the ledger is gone from the
   working tree: read it from the commit before that drop instead,
   `git show HEAD~1:<ledger path>`. A dirty checkout is refused, and the
   refusal says so; leave that one for the user rather than forcing it.

Never merge the PR, add reviewers, or write code on the plan branch from
here: only the fix task, started and watched like any other dispatch
task, changes the repo's own files. The plan files are review's to commit
and push — the fix task's plan entry and prompt file, deferred ledger
lines, the drop before the PR — and only while no task may push the plan
branch: at `COMPLETE`, never while the fix task runs.

## Common mistakes

| Thought | Reality |
|---|---|
| "The check can run on the flock, same as the tasks did." | Run it here, in a local worktree: review's own check is a verification step, not another unattended task. |
| "A second fix task will clear up what the first missed." | One fix task, one scoped re-review. A re-review that still finds Critical findings gets a draft PR with them listed, not another task. |
| "I'll write the fix task's prompt file and start it with `pastor task run` myself." | Put it in the plan first, as Task `<n+1>` with a Dispatch block, and let dispatch run it: `next-task.sh` only knows numbered tasks, and the prompt template sends the agent to the plan for its steps. |
| "A `t-N` argument names the branch to review: the task's own." | The task's branch is local to the machine that ran it and was never pushed. The plan branch is in the task's prompt (`origin/pastor/<name>`), and that is what review fetches. |
| "Review never pushes the plan branch." | Review never pushes code. The plan files — the fix task's entry, deferred lines, the drop before the PR — are its own to commit and push, at `COMPLETE`, when no task can push the branch. |
| "Minor findings should block the PR too." | Only Critical and Important do. Minor findings go to the ledger as deferred and the PR still opens. |
| "I'll add myself or the user as a reviewer so someone double-checks." | Ask for no reviewers. The repo's own gate (CI, branch protection) is what checks a pastor PR. |
| "The PR should merge once it's green." | Review never merges on its own; the user decides when, by the repo's own rule. |

## Worked example

`skills/spec/example/2026-09-26-nested-herdr.md`, the same plan
`pastor:spec` and `pastor:dispatch` are tested against: its two tasks
(`skills/spec/example/2026-09-26-nested-herdr/task-1.md` and `task-2.md`)
give this skill a real Pastor header, Check command and Review Focus to
read through steps 1–3 against. Opening a real pull request from it, or
starting a real fix task, needs a flock and a GitHub remote this worktree
does not have; the checklist above is what a session with both runs.
