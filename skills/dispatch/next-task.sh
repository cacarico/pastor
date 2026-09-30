#!/bin/sh
# skills/dispatch/next-task.sh <plan.md>
#
# The mechanical part of dispatch, shared by the interactive pastor:dispatch
# skill and a scheduled orchestrator's pre script: reads a pastor:spec plan
# (skills/spec/plan-format.md) and its ledger, and prints exactly one line
# naming what to do next.
#
#   RUN <n> <prompt-file>    task n has never been started; start it
#   WAIT <n> t-<id>          task n's agent is still working
#   CHECK <n> t-<id> <state> task n's agent stopped; a human or the caller decides
#   COMPLETE                 every task has its "Task N: complete" ledger line
#   BLOCKED <n> <why>        the ledger itself says task n is blocked
#
# The ledger is read as the sibling file next to <plan.md> whose name is the
# basename of the header's `Ledger:` field: this script does not fetch or
# switch branches, so the caller (the skill, or the orchestrator's checkout)
# is responsible for <plan.md> already being the plan branch's copy.
#
# PASTOR names the pastor binary used to check a live task's state
# (default: pastor on PATH).
set -eu

PASTOR=${PASTOR:-pastor}

plan=${1:-}
if [ -z "$plan" ] || [ ! -f "$plan" ]; then
    echo "usage: next-task.sh <plan.md>" >&2
    exit 2
fi

dir=$(dirname "$plan")

ledger_field=$(sed -n 's/^- Ledger: `\(.*\)`$/\1/p' "$plan" | head -n 1)
if [ -z "$ledger_field" ]; then
    echo "next-task.sh: $plan has no Pastor header Ledger field" >&2
    exit 2
fi
ledger="$dir/$(basename "$ledger_field")"

total=$(grep -c '^### Task [0-9][0-9]*:' "$plan" 2>/dev/null || true)
total=${total:-0}
if [ "$total" -eq 0 ]; then
    echo "next-task.sh: $plan has no Task headings" >&2
    exit 2
fi

# One pass over the ledger: for the first task with no "complete" line, its
# last event (if any) decides RUN, BLOCKED or PENDING (a live task, checked
# below). The ledger is append-only, so the last event per task wins.
decision=$(
    if [ -f "$ledger" ]; then cat "$ledger"; fi | awk -v total="$total" '
        /^Task [0-9]+: complete/ {
            n = $0
            sub(/^Task /, "", n)
            sub(/:.*/, "", n)
            complete[n + 0] = 1
            next
        }
        /^Task [0-9]+: blocked:/ {
            n = $0
            sub(/^Task /, "", n)
            sub(/:.*/, "", n)
            n = n + 0
            why = $0
            sub(/^Task [0-9]+: blocked: /, "", why)
            kind[n] = "blocked"
            detail[n] = why
            next
        }
        /^Task [0-9]+: ran as t-[0-9]+ on / {
            n = $0
            sub(/^Task /, "", n)
            sub(/:.*/, "", n)
            n = n + 0
            id = $0
            sub(/.*ran as t-/, "", id)
            sub(/ on .*/, "", id)
            kind[n] = "ran"
            detail[n] = id
            next
        }
        END {
            target = 0
            for (n = 1; n <= total; n++) {
                if (!(n in complete)) { target = n; break }
            }
            if (target == 0) {
                print "COMPLETE"
                exit
            }
            if (kind[target] == "blocked") {
                print "BLOCKED", target, detail[target]
            } else if (kind[target] == "ran") {
                print "PENDING", target, detail[target]
            } else {
                print "RUN", target
            }
        }
    '
)

kind=${decision%% *}
case "$kind" in
    COMPLETE)
        echo COMPLETE
        exit 0
        ;;
    BLOCKED)
        rest=${decision#BLOCKED }
        target=${rest%% *}
        why=${rest#* }
        echo "BLOCKED $target $why"
        exit 0
        ;;
    RUN)
        target=${decision#RUN }
        prompt_file=$(awk -v want="$target" '
            /^### Task [0-9]+:/ {
                n = $0
                sub(/^### Task /, "", n)
                sub(/:.*/, "", n)
                task = n + 0
                in_dispatch = 0
                next
            }
            task == want && /^\*\*Dispatch:\*\*/ { in_dispatch = 1; next }
            task == want && in_dispatch && /^```bash/ { capturing = 1; next }
            capturing && /^```/ { capturing = 0 }
            capturing {
                line = $0
                sub(/\\$/, "", line)
                buf = buf " " line
            }
            END {
                n = split(buf, words)
                for (i = 1; i <= n; i++) {
                    if (words[i] == "--prompt-file") { print words[i + 1]; exit }
                }
            }
        ' "$plan")
        if [ -z "$prompt_file" ]; then
            echo "next-task.sh: task $target has no --prompt-file in its Dispatch block" >&2
            exit 2
        fi
        echo "RUN $target $prompt_file"
        exit 0
        ;;
    PENDING)
        rest=${decision#PENDING }
        target=${rest%% *}
        id=${rest#* }
        state=$("$PASTOR" task describe "t-$id" --json) || {
            echo "next-task.sh: $PASTOR task describe t-$id failed" >&2
            exit 1
        }
        task_state=$(printf '%s\n' "$state" | sed -n 's/.*"state": *"\([^"]*\)".*/\1/p' | head -n 1)
        if [ -z "$task_state" ]; then
            echo "next-task.sh: could not read t-$id's state" >&2
            exit 1
        fi
        case "$task_state" in
            queued | starting | running | paused | waiting)
                echo "WAIT $target t-$id"
                ;;
            *)
                echo "CHECK $target t-$id $task_state"
                ;;
        esac
        exit 0
        ;;
    *)
        echo "next-task.sh: could not read the ledger" >&2
        exit 1
        ;;
esac
