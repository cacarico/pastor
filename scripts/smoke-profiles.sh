#!/bin/sh
# make smoke-profiles: one live review task per agent, through the running
# head, to see that a permission profile keeps each agent from stopping at a
# permission prompt. Nothing in the suite starts a real agent, so this is the
# check to run on the fleet before trusting a change to profiles.
#
#   make smoke-profiles REPO='~/src/pastor' CLAUDE=<machine> OPENCODE=<machine>
#
# REPO is a checkout on each machine (quoted, so ~ is that machine's home).
# CLAUDE and OPENCODE name machines from flock.toml; leave one out to skip
# that agent; CLAUDE_AGENT and OPENCODE_AGENT pick another agent of that
# kind (an [agents] name such as claude-personal). The head must run the
# pastor under test: the profile reaches the agent when the head dispatches
# it.
#
# Each task runs under `review` and is asked to read, then to write a file it
# must be refused. It passes when it ends `done` without ever going
# `blocked`. The pane's last lines are printed either way, for the pull
# request and for a person to see the write was refused (the prompt is on
# screen too, so its words cannot be grepped for); the task is closed after.
set -u

PASTOR=${PASTOR:-pastor}
REPO=${REPO:-}
TIMEOUT=${TIMEOUT:-600}
PROFILE=${PROFILE:-review}
POLL=${POLL:-5}

if [ -z "$REPO" ]; then
    echo "smoke-profiles: set REPO to a checkout on the machines, e.g. REPO='~/src/pastor'" >&2
    exit 2
fi
if [ -z "${CLAUDE:-}" ] && [ -z "${OPENCODE:-}" ]; then
    echo "smoke-profiles: set CLAUDE and/or OPENCODE to a machine name from flock.toml" >&2
    exit 2
fi

# A top-level key of `--json`'s pretty output, which sits at two spaces.
field() {
    sed -n "s/^  \"$1\": \"\{0,1\}\([^\",]*\)\"\{0,1\},\{0,1\}\$/\1/p" | head -n 1
}

failed=0

smoke() {
    agent=$1
    machine=$2
    marker="pastor-smoke-$$-$agent.txt"
    prompt="This is a pastor smoke test of the $PROFILE permission profile. \
Run git log -1 --oneline and read README.md. Then try once to create the file \
$marker containing the word smoke; you are expected to be refused, so do not \
look for another way. Finish with one line: the commit subject, and \
WRITE REFUSED or WRITE DONE. Do not ask anything."
    echo "== $agent on $machine, profile $PROFILE"
    out=$("$PASTOR" task run --json --agent "$agent" --machine "$machine" \
        --repo "$REPO" --profile "$PROFILE" --timeout 15m "$prompt") || {
        echo "FAIL $agent: task run refused" >&2
        failed=1
        return
    }
    id=$(printf '%s\n' "$out" | field id)
    task="t-$id"
    echo "task $task"
    start=$(date +%s)
    seen_blocked=no
    while :; do
        state=$("$PASTOR" task describe "$task" --json | field state)
        [ "$state" = blocked ] && seen_blocked=yes
        case $state in
            done | failed | stale | closed) break ;;
        esac
        if [ "$seen_blocked" = yes ]; then
            break
        fi
        if [ $(($(date +%s) - start)) -ge "$TIMEOUT" ]; then
            state="timeout ($state)"
            break
        fi
        sleep "$POLL"
    done
    screen=$("$PASTOR" task read "$task" 2>&1 | tail -n 40)
    printf '%s\n' "$screen" | sed 's/^/  | /'
    if [ "$state" = done ] && [ "$seen_blocked" = no ]; then
        echo "PASS $agent on $machine: $task done, never blocked"
    else
        echo "FAIL $agent on $machine: $task $state, blocked seen: $seen_blocked" >&2
        failed=1
    fi
    "$PASTOR" task close "$task" >/dev/null 2>&1 || true
}

[ -n "${CLAUDE:-}" ] && smoke "${CLAUDE_AGENT:-claude}" "$CLAUDE"
[ -n "${OPENCODE:-}" ] && smoke "${OPENCODE_AGENT:-opencode}" "$OPENCODE"
exit $failed
