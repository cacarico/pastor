---
name: dispatch
description: "Runs a pastor:spec plan's next task and watches it to a decision point: starts the next task, waits with no tight loop, and handles a blocked folder-trust prompt, a done task's outcome, a failed task with a known cause, and a stale task by fixed rules. Anything the rules do not cover goes to the user, never guessed. Disabled for automatic invocation: it starts agents on other machines, so a person picks when."
disable-model-invocation: true
model: sonnet
effort: medium
argument-hint: "[plan path]"
---

# pastor:dispatch

Carries a `pastor:spec` plan through, one task at a time, with nobody else
watching the branch: starts the next task, waits for it, and handles
`blocked`, a `done` task's outcome, `failed` and `stale` by fixed rules.
Anything the rules do not cover goes to the user — never guess past it.

**REQUIRED BACKGROUND:** [plan-format.md](../spec/plan-format.md) for the
plan, its Dispatch blocks and the ledger this skill drives, and the `pastor`
skill (or `pastor --skill`) for the CLI. Neither is repeated here.

Opens with the live state:

!`pastor machine list --json`
!`pastor task list --json`

## Checklist

Work from a local checkout of the plan branch, `pastor/<name>`, as the
plan's own Dispatch commands expect: their prompt files are relative to the
repo root, and `next-task.sh` reads the plan and the ledger from disk, never
from the remote. The helper lives with this skill, not with the plan, so a
plan in another repository has no `skills/dispatch/` of its own: it is
`${CLAUDE_SKILL_DIR}/next-task.sh` (Claude Code fills in the directory this
SKILL.md was installed from, as the `pastor` plugin or linked by hand), or
`skills/dispatch/next-task.sh` in a checkout of pastor's own repository.
Either way, run it from the plan checkout, with the plan's path as its
argument.

1. **Refuse** a plan with no Dispatch blocks (writing them is `pastor:spec`'s
   job), no plan branch in its Pastor header, or an `Order` other than
   `series`: `next-task.sh` reads `### Task <n>:` headings by number alone
   and does not understand waves or merge tasks.
2. **Ask `next-task.sh <plan.md>`** what is next, and act on exactly the one
   line it prints:
   - `RUN <n> <prompt-file>`: read `pastor flock list --json` and
     `pastor machine list --json` for a machine in the task's flock that is
     `connected` or `polling`, carries every tag the Dispatch block asks
     for, and has a free slot (its agent count short of `max_agents` plus
     any job slots and burst the task may use). Only once that holds, run
     task `<n>`'s Dispatch command exactly as the plan wrote it. Take the
     `id` and `machine` from its `--json` output (a task still `queued` has
     no machine yet; `pastor task describe t-<id> --json` once it has
     started), then record it: fetch, rebase onto the plan branch, append
     `Task <n>: ran as t-<id> on <machine>` to the ledger, commit and push
     — retry the fetch-rebase-push once on a rejected push, the same way
     the plan's own prompts do. That line is what makes the next
     `next-task.sh` call, even from a fresh session, find the task
     `WAIT`ing rather than unstarted; it watches whichever id the last
     `ran as` line for task `<n>` names, so a replacement task (step 4's
     failed and stale rules) gets a line of its own the same way.
   - `WAIT <n> t-<id>`: the task is already live. Go to step 3.
   - `CHECK <n> t-<id> <state>`: the task has stopped and needs a decision.
     Go to step 4.
   - `COMPLETE`: every task has its ledger line. Go to step 5.
   - `BLOCKED <n> <why>`: the ledger itself says the task is blocked. Show
     the user `<why>`; dispatch does not decide for them.
3. **Wait**, never in a tight loop: `/loop` at 5 minutes, or
   `timeout 300 pastor events --task t-<id> --follow`. Either way, once it
   returns, go back to step 2.
4. **The four rules, in order.** Apply the first that matches; anything else
   goes to the user with the pane text, never answered on their behalf.
   - **Blocked on the folder-trust prompt:** `pastor task send t-<id>
     --trust`, then go back to step 3. Any other `blocked`: read the pane
     with `pastor task read t-<id>`, show the user that text, and stop.
   - **Done, or closed.** A done task is auto-closed once `close_done_after`
     has passed (five seconds by default), so the wait usually returns to a
     `closed` task, not a `done` one; its summary stays with it, and the
     rule is the same. `pastor task describe t-<id> --json` for the
     summary. Outcome `done`: `git fetch origin` and check that the plan
     branch moved and its ledger, as fetched (`git show
     origin/pastor/<name>:<ledger path>`), has `Task <n>: complete`. Only
     once both hold, fast-forward the plan checkout (`git merge --ff-only
     origin/pastor/<name>`), so `next-task.sh` reads the new line, and go
     back to step 2. No push, no `complete` line, an outcome of `partial`
     or `blocked`, or a `closed` task with no summary at all (closed by
     hand before it finished) is a failure — report it and let the user
     decide.
   - **Failed with a known cause** (`agent_kind_missing`, `agent_pane_busy`,
     a repo path that does not exist on the machine): fix the cause, then
     `pastor task retry t-<id> --json`. The retry is a new task with a new
     `id`; the failed row stays as it is. Record the new id and machine with
     a `Task <n>: ran as t-<new id> on <machine>` ledger line, as step 2's
     `RUN` handling does, and go back to step 2; without that line
     `next-task.sh` keeps reporting the old failure and would retry it
     again. Any other cause goes to the user.
   - **Stale:** its agent may still be at work, and a series task pushes
     the plan branch, so nothing starts the task again while that agent
     may push. First `git fetch origin` and look at the fetched ledger: a
     stale task that has already pushed its `Task <n>: complete` line is
     finished, and the done rule applies, with `pastor task close t-<id>
     --remove-worktree` to take its pane. Otherwise read the pane with
     `pastor task read t-<id>`, keep that text for the user, and `pastor
     task close t-<id> --remove-worktree`: that stops the agent and frees
     the checkout and its branch. A refused close (a dirty checkout) goes
     to the user with the refusal and the pane text; do not force it. Once
     closed, start task `<n>` again exactly as step 2's `RUN` handling
     does: its Dispatch command, then a new `ran as` line. Not `pastor task
     retry`: a closed task is not retryable, and a retry before the close
     would run beside an agent that may still push the plan branch.
5. **On `COMPLETE`**, tell the user to run `/pastor:review` next, and stop.

Never: `pastor serve`, `pastor setup`, `pastor machine add`, `pastor machine
remove` or `pastor task prune`. Dispatch starts and watches tasks on an
existing flock; it never changes the fleet.

## Common mistakes

| Thought | Reality |
|---|---|
| "I'll start the next task now; the ledger will catch up." | `Order: series` starts task N+1 only once the ledger has `Task N: complete`. Wait for step 4's done rule to confirm it, not just a `done` state. |
| "A `done` task is finished." | `done` only means the agent went idle. Check the branch moved and the ledger line is there before trusting it; a trailing question reads as `done` too until the pane is checked. |
| "`CHECK ... closed` means someone closed it, so something went wrong." | Auto-close takes a done task after `close_done_after`, five seconds by default, so a task that finished during the wait reads `closed`. Its summary is still there: apply the done rule to it. |
| "`git fetch` is enough; `next-task.sh` will see the new ledger line." | It reads the checkout on disk, and a fetch only moves `origin/pastor/<name>`. Fast-forward the plan checkout after the done rule's checks pass, not before them. |
| "Any `failed` task is safe to retry." | Only a known cause. An unknown one goes to the user — retrying blindly repeats whatever broke it. |
| "`pastor task retry` reruns t-<id>, so the ledger is still right." | A retry is a new task with a new id; the failed row stays failed. Without a new `ran as` line, `next-task.sh` keeps reporting the old failure and would retry it again. |
| "A `stale` task can be retried like a failed one." | Its agent may still be at work, and could still push the plan branch beside the retry. Close it first; a closed task is not retryable, so start the task again from its Dispatch command. |
| "I can poll `next-task.sh` every few seconds to catch the moment it finishes." | That is a tight loop. Wait with `/loop` at 5 minutes or `pastor events --follow`. |
| "A plan with `Order: waves` just runs one task at a time, same as series." | Refuse it. `next-task.sh` has no idea a wave or a merge task exists. |
| "I should answer the question the agent left `blocked` on." | Only the folder-trust prompt is dispatch's to answer. Any other question goes to the user. |

## Worked example

`skills/spec/example/2026-09-26-nested-herdr.md` and its ledger are the same
worked plan `pastor:spec`'s own tests use. `next-task.sh` already runs
against it end to end in `tests/cli/spec_skill.rs`
(`next_task_reads_the_example_plan_and_its_ledger`): `RUN 1 ...` before
anything has happened, `WAIT 1 t-14` while its agent is still `running`,
`CHECK 1 t-14 done` once it stops, `RUN 2 ...` once the ledger has `Task 1:
complete`, and `COMPLETE` once both tasks do. This skill's own checklist
walks that same plan one Dispatch block at a time; starting a real task
against a live flock is this skill's job to do when invoked, not this
worktree's to do while writing it.
