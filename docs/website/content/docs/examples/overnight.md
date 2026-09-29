---
title: keep PRs moving overnight
summary: a night watch that merges what is ready
weight: 7
---
Agents open PRs all day; at night nobody merges them. A night watch keeps
them moving while you sleep. Every five minutes a script merges what is
ready and sends fix agents to what is broken. A model wakes only for what
the script cannot decide, so most rounds cost nothing.

## what you need

- `gh` logged in on the head, with the right to merge on `owner/app`, and
  `jq`.
- A model named `sonnet` under `[models]` in `pastor.toml`
  ([a PR fix round](../pr-fix-round/#what-you-need) shows the entry).
- A `local = true` machine in `flock.toml`. The orchestrator's own agent
  runs only on the head's machine, and takes one of its slots.
- `~/src/app` cloned on the head and on the machines that take fix tasks.

## the orchestrator

```toml
# ~/.config/pastor/orchestrators/night.toml
kind = "scheduled"
description = "Merges the night's ready PRs on the app"
cron = "*/5 22-23,0-7 * * *"
pre = ["./night-pre.sh"]
model = "sonnet"
repo = "~/src/app"
prompt = """
Each line below is a PR or a task the night watch could not handle alone.
Answer a blocked task with pastor task send, reply to or resolve review
threads, or start a fix with pastor task run. Leave to a person what needs
one, and say so in the handover note.
"""
```

`cron` is in the head's local time: every five minutes from 22:00 to 07:59.
`pre` is relative to the file. The script runs on the head with the
orchestrator's rights: it may run, retry, send to and close tasks, and
nothing else that changes a flock. It gets a scratch directory of its own
in `PASTOR_ORCHESTRATOR_STATE_DIR`, kept between rounds.

<details>
<summary>the pre script, night-pre.sh</summary>

```sh
#!/bin/sh
# ~/.config/pastor/orchestrators/night-pre.sh
# One round of the night watch: do the mechanical work, print the rest.
set -eu
repo=owner/app
dir=$PASTOR_ORCHESTRATOR_STATE_DIR

# The state of the newest task with this description, empty if none.
task_state() {
  pastor task list --json |
    jq -r --arg d "$1" '[.[] | select(.description == $d)] | max_by(.id) | .state // empty'
}
live() { case "$1" in queued|starting|running|blocked|paused) return 0 ;; esac; return 1; }

# Open review threads on a PR, over every page of them.
unresolved() {
  gh api graphql --paginate -F owner="${repo%/*}" -F name="${repo#*/}" -F pr="$1" -f query='
    query($owner: String!, $name: String!, $pr: Int!, $endCursor: String) {
      repository(owner: $owner, name: $name) { pullRequest(number: $pr) {
        reviewThreads(first: 100, after: $endCursor) {
          nodes { isResolved } pageInfo { hasNextPage endCursor } } } } }' \
    --jq '[.data.repository.pullRequest.reviewThreads.nodes[] | select(.isResolved | not)] | length' |
    awk '{ n += $1 } END { print n + 0 }'
}

# One line per open PR: number, head sha, merge state, review, checks.
# No checks reported yet counts as PENDING, never GREEN.
gh pr list --repo "$repo" --search draft:false --limit 200 \
  --json number,headRefOid,mergeStateStatus,reviewDecision,statusCheckRollup \
  --jq '.[] | "\(.number) \(.headRefOid[0:7]) \(.mergeStateStatus) \(.reviewDecision // "" | if . == "" then "NONE" else . end) \(
    [.statusCheckRollup[] | (.conclusion // "") as $c | if $c != "" then $c else (.state // "PENDING") end]
    | if length == 0 then "PENDING"
      elif any(. == "FAILURE" or . == "ERROR" or . == "TIMED_OUT" or . == "CANCELLED") then "FAILING"
      elif all(. == "SUCCESS" or . == "NEUTRAL" or . == "SKIPPED") then "GREEN"
      else "PENDING" end)"' >"$dir/prs"

# The PR whose rebase is in flight; it holds the others until it merges.
rebasing=$(cat "$dir/rebasing" 2>/dev/null || true)
if [ -n "$rebasing" ] && ! grep -q "^$rebasing " "$dir/prs"; then
  rm -f "$dir/rebasing"
  rebasing=
fi

merged=no
while read -r pr sha merge review checks <&3; do
  if [ "$merge" = DIRTY ] || [ "$checks" = FAILING ]; then
    [ "$pr" = "$rebasing" ] && rm -f "$dir/rebasing" && rebasing=
    what="fix PR #$pr at $sha"
    if [ ! -e "$dir/sent-$pr-$sha" ]; then
      pastor task run "Check out PR #$pr with gh pr checkout $pr, rebase it on main, fix what conflicts or fails, run the tests and push." \
        --repo '~/src/app' --worktree --model sonnet --description "$what" >&2
      touch "$dir/sent-$pr-$sha"
    elif ! live "$(task_state "$what")"; then
      echo "PR #$pr: $what ended with nothing pushed"
    fi
  elif [ "$review" = CHANGES_REQUESTED ]; then
    echo "PR #$pr: changes requested"
  elif [ "$review" != APPROVED ] || [ "$checks" != GREEN ]; then
    : # waiting on a reviewer or on CI
  elif [ "$(unresolved "$pr")" != 0 ]; then
    echo "PR #$pr: approved, with unresolved review threads"
  elif [ "$merge" = CLEAN ] && [ "$merged" = no ]; then
    # One merge a round: the next one's state is stale until GitHub catches up.
    if gh pr merge "$pr" --repo "$repo" --squash --delete-branch >&2; then
      merged=yes
    else
      echo "PR #$pr: approved and green, but the merge was refused"
    fi
  elif [ "$merge" = BEHIND ] && [ -z "$rebasing" ]; then
    # A rebase that conflicts is refused; the PR shows DIRTY next round
    # and gets a fix agent then.
    if gh pr update-branch "$pr" --repo "$repo" --rebase >&2; then
      echo "$pr" >"$dir/rebasing"
      rebasing=$pr
    fi
  elif [ "$merge" = BLOCKED ]; then
    echo "PR #$pr: approved and green, but GitHub still blocks the merge"
  fi
done 3<"$dir/prs"

# A blocked task needs an answer.
pastor task list --json |
  jq -r '.[] | select(.state == "blocked") | "TASK t-\(.id) blocked: \(.description)"'
```

</details>

Make it executable with `chmod +x`. Everything the script prints on stdout
is a line for the agent; what it does itself goes to stderr.

## one round

1. If the last round's agent still works, the round is skipped.
2. The script reads the open PRs once.
3. A PR that conflicts with main or fails its checks gets one fix agent per
   commit, in its own worktree. If that agent ends without pushing, the next
   round prints a line about it.
4. The first approved, green PR with no open threads is merged. One merge a
   round, because the next PR's state is stale until GitHub catches up.
5. One PR behind main is rebased. The others wait until it merges or needs a
   fix, so a merge never leaves the next one stale while it waits for CI.
6. What is left is printed: approved PRs with open threads, changes
   requested, a merge GitHub refused, a fix that ended with nothing pushed,
   a blocked task.
7. No lines, no agent. With lines, pastor starts one agent on the head's
   machine with the orchestrator role, the handover note and every line in
   its prompt.

## what you see

Try one round by hand before the night. `orchestrator run` ignores the
schedule:

```sh
pastor orchestrator run night
pastor orchestrator list
```

```text
NAME   KIND       STATE    SCHEDULE                  LAST RUN  NEXT RUN  AGENT  LAST RESULT
night  scheduled  running  cron */5 22-23,0-7 * * *  1m ago    in 4m     t-31   started t-31
```

A quiet round reads `no lines` under LAST RESULT. The agent and the fix
agents are ordinary tasks. While an orchestrator agent is live,
`pastor task list` shows it in a table of its own:

```text
orchestrators:
ID    STATE    PRIORITY  MACHINE  FLOCK    AGENT   MODEL   JOB  AGE  NOTE
t-31  running  normal    desk     default  claude  sonnet  run  1m   Each line below is a PR or a task the night watch could not

tasks:
ID    STATE    PRIORITY  MACHINE   FLOCK    AGENT   MODEL   JOB  AGE  NOTE
t-30  running  normal    server-1  default  claude  sonnet  run  4m   Check out PR #44 with gh pr checkout 44, rebase it on main,
```

In the morning, `pastor orchestrator describe night` shows the last rounds
with the lines each printed, the handover note and recent events.

## next

- [orchestrators](../../concepts/orchestrators/): scheduled and session kinds, the role
- [let your agent drive pastor](../agent-drives/): what the orchestrator's agent may do
- [cli: orchestrator](../../reference/cli/#orchestrator)
