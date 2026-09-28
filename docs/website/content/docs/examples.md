---
title: examples
summary: setups taken from real use, to copy
group: use
weight: 24
manual: how-it-works
---

Six setups from how pastor is used every day, with the names swapped for
placeholders: `desk` is the head, `server-1` and `laptop` take tasks,
`~/notes` is a notes vault and `~/src/app` the repo the agents work on.

## a Kanban board that hands cards to agents

A board in an Obsidian vault is the to-do list. A card tagged `#agent` in
the `Ready` list becomes a task in its own worktree, one branch per card.

```toml
# ~/.config/pastor/jobs/kanban.toml
description = "Hand #agent cards on the app board to agents"
every = "1m"

[connector]
use = "obsidian-kanban"
board = "~/notes/Boards/App.md"
list = "Ready"
tag = "#agent"

[dispatch]
repo = "~/src/app"
worktree = true
branch = "kanban/{{ item.key }}"
prompt = """
You are pastor task {{ task.id }}, started from the card "{{ item.title }}".
Do what the card asks, test it, commit and push the branch.
"""
```

The card moves itself: to In Progress when its task starts, In Review when
it is done, Failed when it fails. A card whose `depends-on` names another
card stays where it is until that one is done. Test the job before it runs
for real:

```sh
pastor tick --dry-run --job kanban
```

## answer questions in a note, let an agent carry them out

A second job on the same connector reads the `Answered` list, with no tag:
every card there is a question someone answered, and the agent does what
the answer says.

```toml
# ~/.config/pastor/jobs/answered.toml
description = "Carry out the answers on the app board"
every = "1m"

[connector]
use = "obsidian-kanban"
board = "~/notes/Boards/App.md"
list = "Answered"

[dispatch]
repo = "~/src/app"
worktree = true
branch = "answered/{{ item.key }}"
prompt = "Read the card \"{{ item.title }}\" and its answer, and carry it out."
```

## a PR fix round from one command

Review comments on PR `#42`: one command starts an agent on them, in a
worktree of its own, on a named model from `[models]` in `pastor.toml`.

```sh
pastor task run "Check out PR #42 with gh pr checkout 42, address every review comment, push" \
  --repo '~/src/app' --worktree --model sonnet
```

Then follow it, read what it did, and step in if it asks:

```sh
pastor task list
pastor task read t-12
pastor task attach t-12  # ctrl+b q to leave
```

## a night watch that merges what is ready

An [orchestrator](../orchestrators/) keeps the night's PRs moving. Every
five minutes a pre script merges one PR whose review is done, threads
resolved and checks green, sends a fix agent to one that conflicts with
main or fails its checks, and rebases the ones behind main one at a time, so
a merge never leaves the next one stale while it waits for CI. A model is
woken only for what the script cannot decide: approved PRs with open
threads, changes requested, a merge GitHub refused, a fix that ended with
nothing pushed, a task blocked on a question. Most rounds wake nothing.

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

Each round, in order: the open PRs are read once; a conflicting or failing
PR gets one fix agent per commit, in its own worktree; the first approved,
green PR with no open threads is merged; one PR behind main is rebased and
the others wait until it merges or needs a fix; what is left is printed,
and only then does pastor start the agent, with those lines in its prompt.
The fix agents are ordinary tasks, in `pastor task list` like any other.
Try one round by hand before the night:

```sh
pastor orchestrator run night
pastor orchestrator describe night
```

## two accounts on one head

A personal and a work Claude account, each with its own machines, so the
two never run each other's agents. The work agent is Claude with its own
login:

```toml
# ~/.config/pastor/pastor.toml
[agents.claude-work]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
```

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "personal"
default = true
agent = "claude"

[[flock]]
name = "work"
agent = "claude-work"

[[machine]]
name = "desk"
local = true
flock = "personal"

[[machine]]
name = "laptop"
ssh = "user@laptop"
flock = "work"
```

A task names the flock it belongs to; one that names none goes to
`personal`:

```sh
pastor task run "Fix the login redirect" --repo '~/src/app' --worktree --flock work
```

## agents on another machine over ssh

The head runs agents on itself and on `server-1`, which it reaches over ssh
with a key and no password. Each task goes to the one with the fewest live
tasks that still has room:

```sh
pastor machine add server-1 user@server-1 --max-agents 3 --herdr
pastor machine list
```

```text
pastor 0.8.0 on desk (herdr 0.9.1), 2 machines, desk is the head of the flock

NAME      HOST           FLOCKS    PROFILE  CHANNEL    HERDR  PASTOR  AGENTS     ORPHANS  TAGS  ERROR
desk      local          personal  -        connected  0.9.1  0.8.0   1/2+1j+1b  -        -
server-1  user@server-1  personal  -        connected  0.9.1  0.8.0   2/3+1j+1b  -        -
```

AGENTS is the load: live tasks against the room each machine has. To run the
CLI from the laptop against the head on `desk`, see [remote head](../remote-head/).

More in the [manual](../manual/#how-it-works).
