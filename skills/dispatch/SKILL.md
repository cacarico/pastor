---
name: dispatch
description: "Not yet the full pastor:dispatch skill: only its mechanical part exists so far, skills/dispatch/next-task.sh. A later card adds the interactive skill (running a pastor:spec plan, handling blocked, done, failed and stale) and the scheduled-orchestrator mode; until then, disabled for model invocation."
disable-model-invocation: true
---

# pastor:dispatch (mechanical part only)

This skill is not built yet. What exists so far is `skills/dispatch/next-task.sh <plan.md>`, the shared mechanical part a later `pastor:dispatch` skill and a scheduled orchestrator's pre script will both call.

**REQUIRED BACKGROUND:** `skills/spec/plan-format.md` for the plan and ledger the script reads.

It reads the plan's Pastor header (for the ledger's path, resolved next to `<plan.md>`) and the ledger, and prints exactly one line:

- `RUN <n> <prompt-file>`: task `<n>` has never been started; its Dispatch block's `--prompt-file` is `<prompt-file>`.
- `WAIT <n> t-<id>`: task `<n>`'s agent, running as `t-<id>`, is still working (`pastor task describe t-<id> --json`'s `state` is `queued`, `starting`, `running`, `paused` or `waiting`).
- `CHECK <n> t-<id> <state>`: task `<n>`'s agent stopped in `<state>` and needs a decision.
- `COMPLETE`: every task has its `Task N: complete` ledger line.
- `BLOCKED <n> <why>`: the ledger itself has `Task N: blocked: <why>` as the task's last event.

The script does not fetch or switch branches: the caller is responsible for `<plan.md>` already being the plan branch's own copy, and for the `pastor` binary on `PATH` (or `$PASTOR`) reaching the right head.
