---
name: spec
description: "Turns an idea or an approved design into a plan that a pastor flock can run with nobody watching: every task gets a flock or machine, repo, branch, model, timeout and a self-contained prompt, and the plan, its prompts and a ledger live on a plan branch so any machine can resume. Use when the user wants work planned for pastor or the flock (\"spec this for pastor\", \"plan this so the flock can run it\", \"split this into tasks for the Pis\"), or when a design is approved and its execution will not happen in this session."
argument-hint: "[topic or spec path]"
---

# pastor:spec

Plan work so that pastor can run it, one task after another, on machines where no human is watching.

**REQUIRED BACKGROUND:** the `pastor` skill (or `pastor --skill`). This skill does not repeat the CLI.

**REQUIRED SUB-SKILLS:** `superpowers:brainstorming` for the design, `superpowers:writing-plans` for the task breakdown. This skill adds what an unwatched agent needs on top of them.

## Why this is different from a normal plan

A pastor task gets its prompt and nothing else. Nobody answers its questions: a question leaves it `blocked` until someone happens to attach. `done` means the agent went idle, not that the work is good. After `close_done_after` pastor removes a clean worktree, so **the pushed branch is the only state that survives**. A task that has not pushed has not finished.

So every task in the plan must:

- say where the agent is and how to get the latest work (fetch and reset to the plan branch),
- carry everything the agent needs to decide, with no "ask the user",
- end by committing, then fetching and rebasing onto the plan branch, writing its ledger line, pushing the plan branch (once more after a rebase if the push is rejected), and printing `DONE`.

## Checklist

Follow these in order. Create a todo for each.

1. **Design.** If the user has no approved spec, run `superpowers:brainstorming` unchanged, approval gate included. Add nothing until the design is approved. This is the only step where you ask the user things; ask here what the tasks cannot, including the repo path on the target machines (`--repo` is a path there, not here) and the command that checks the repo (`make check`, `cargo test`, ...), unless the repo says.
2. **Read the fleet.** `pastor machine list --json` and `pastor flock list --json`: flocks, tags, `max_agents`, which machines are `connected` or `polling`. With no head, the probe answer is enough. Never start `pastor serve` or install services.
3. **Name the plan.** `<name>` is short kebab case. The plan branch is `pastor/<name>`. Files, all on that branch:
   - `docs/superpowers/plans/YYYY-MM-DD-<name>.md`: the plan
   - `docs/superpowers/plans/YYYY-MM-DD-<name>.ledger.md`: the ledger
   - `docs/superpowers/plans/YYYY-MM-DD-<name>/task-N.md`: one prompt file per task
4. **Write the plan** with `superpowers:writing-plans` as the base and the additions in [plan-format.md](plan-format.md): a Pastor header, and a Dispatch block per task. Tasks run in series on the plan branch; each depends on the one before.
5. **Pick a model per task** from the table below, with a one-line reason in the Dispatch block.
6. **Write one prompt file per task** from the template in [plan-format.md](plan-format.md). The Dispatch command runs it with `pastor task run --prompt-file`, so quotes, backticks and `$` need no escaping.
7. **Self-review.** Everything writing-plans checks, plus the unattended checks below. Fix what fails before going on.
8. **Commit and push the plan branch**: plan, empty ledger (header only) and prompt files, one commit, `git push origin HEAD:pastor/<name>`. Do not push the default branch.
9. **Hand off.** Show the user the plan path, the branch, and the first task's Dispatch command. Stop. The user reviews the plan and starts the first task; each next task starts only once the ledger on the plan branch says the one before is `complete`. Tell the user not to push the plan branch while a task may still push it: the `ran as t-M` ledger line goes in between tasks.

Never run `pastor task run` from this skill. Planning has no side effects on the fleet.

## Model per task

pastor passes the model to Claude with `--agent-arg --model --agent-arg <alias>`. Name it on every task; do not rely on defaults. The aliases below are the ones `claude --help` documents for `--model` (`fable`, `opus`, `sonnet`), each naming the latest model of its family. If the Claude Code on the flock is older and rejects an alias, use the full model id instead, such as `claude-fable-5-1`.

| Task shape | Model | Why |
|---|---|---|
| The plan holds the full code; one or two files; the test is named | `sonnet` | Transcription plus tests. |
| Several files, integration, following existing patterns | `opus` | Judgment across files; a wrong guess costs a retry on a remote machine. |
| Investigation, root cause, unknown scope | `fable` | Long unattended sessions that verify often; a wrong answer costs the most here. |

`sonnet` is the floor. A cheaper model takes more turns, and with nobody watching an agent that stalls becomes a `stale` or `blocked` task and a human's time. If the user's flock runs a different agent, use that agent's own model flag and say so in the plan header.

## Timeouts

`--timeout` bounds the task; past it the task goes `stale`. Give a `sonnet` transcription task `45m`, a multi-file `opus` task `90m`, an investigation `3h`. A task that needs more than that is two tasks.

## Unattended checks

Read each prompt file as a stranger who has nothing else. Every answer must be yes.

- Does it name the repo checkout it runs in, and start with `git fetch origin` and `git reset --hard origin/pastor/<name>`?
- Does it point at the plan file and its own task by number, and tell the agent to read only that task?
- Can the agent finish without asking anything? No "confirm with the user", no "if unsure, ask".
- Does it say what to do when something is missing: write `Task N: blocked: <why>` in the ledger, commit, push, stop?
- Does it name the check to run and forbid committing while it fails?
- Does it fetch and rebase onto `origin/pastor/<name>` right before the ledger commit, push with `git push origin HEAD:pastor/<name>`, retry once on a rejected push, keep both sides of a ledger conflict, and end with `DONE` as the last line?
- Does it forbid touching the default branch, other branches and other worktrees?

And for the plan as a whole:

- Every task has a Dispatch block with a command that `pastor task run --help` accepts.
- Every `--branch` is `pastor/<name>-<N>`, one per task, so a checkout left from the task before never blocks the next.
- No machine addresses, user names, tokens or home paths in anything committed. `--repo '~/work/app'` is fine; `~` is quoted so the local shell does not expand it.

## Red flags

| Thought | Reality |
|---|---|
| "The agent can ask if it gets stuck." | Nobody is there. It goes `blocked` and waits for hours. |
| "It will obviously push when done." | Say it. A task that did not push lost its work when the worktree was removed. |
| "`haiku` is enough for this." | Extra turns unattended cost more than the tokens saved. `sonnet` is the floor. |
| "Two tasks can share one branch name." | The previous task's worktree may still hold it. One `--branch` per task. |
| "I'll run the first task to check it works." | This skill plans. The user starts the tasks. |
| "Tasks 2 and 3 are independent, run them together." | Series first. Parallel waves are not supported yet. |

## Worked example

[example/2026-09-26-nested-herdr.md](example/2026-09-26-nested-herdr.md) is a full plan for a small real pastor change, with its ledger and prompt files next to it.
