#!/bin/sh
# make smoke-rc: `make smoke` (and `make smoke-profiles` when asked) against a
# release candidate, on the machine that runs it. Those are the only checks
# against a real herdr and real agents, and nobody runs them before a
# release unless something does it for them.
#
#   make smoke-rc TAG=v0.9.0-rc.1 SESSION=default LABEL=fleet-arm64
#   scripts/smoke-rc.sh <tag> [session] [label]
#
# The tag is checked out in a scratch worktree under $TMPDIR, never in the
# caller's checkout, built there, and smoked there; the worktree is removed
# on exit. LABEL names the machine in the report: pick one that is not the
# hostname, since the report is pasted into pull requests and cards.
#
# PROFILES=1 with REPO and CLAUDE and/or OPENCODE also runs `make
# smoke-profiles` (see scripts/smoke-profiles.sh). The head hands the agent
# its profile, so it must run the tag under test: the script refuses unless
# the running head answers with the tag's version. It asks the head, not the
# CLI: `$PASTOR serve status --json` here, or over ssh on the head that
# `$PASTOR head show` names. PASTOR (default `pastor`) is the CLI that
# smoke-profiles uses too, and is passed on to it.
#
# Prints one Markdown block: the tag, the label, the herdr version, pass or
# fail per suite, and the last lines of each failure, with the worktree
# path, the home directory and the hostname scrubbed. Exits 0 when every
# suite run passed, 1 when one failed, 2 when it could not start.
set -u

TAG=${1:-}
SESSION=${2:-default}
LABEL=${3:-unnamed machine}
PROFILES=${PROFILES:-}
PASTOR=${PASTOR:-pastor}
REMOTE=${REMOTE:-origin}
TAIL=${TAIL:-30}

say() { echo "smoke-rc: $*" >&2; }

# The value of a string field in `head show --json` (one field per line).
field() {
    sed -n "s/^ *\"$1\": \"\(.*\)\",\{0,1\}\$/\1/p"
}

# The version of the head `$PASTOR task run` talks to, from its answer to a
# ping; empty when no head answers, or when what answers is a headless serve.
head_version() {
    show=$("$PASTOR" head show --json 2>/dev/null) || return 0
    if printf '%s\n' "$show" | grep -q '"remote": true'; then
        dest=$(printf '%s\n' "$show" | field ssh)
        bin=$(printf '%s\n' "$show" | field pastor)
        [ -n "$dest" ] || return 0
        status=$(ssh -o BatchMode=yes "$dest" "${bin:-pastor} serve status --json" 2>/dev/null)
    else
        status=$("$PASTOR" serve status --json 2>/dev/null)
    fi
    printf '%s\n' "$status" | grep -q '"role":"head"' || return 0
    printf '%s\n' "$status" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p'
}

if [ -z "$TAG" ]; then
    say "usage: scripts/smoke-rc.sh <tag> [session] [label]"
    exit 2
fi
top=$(git rev-parse --show-toplevel) || exit 2
cd "$top" || exit 2

git fetch --quiet --tags "$REMOTE" || {
    say "git fetch --tags $REMOTE failed"
    exit 2
}
if ! git rev-parse --verify --quiet "refs/tags/$TAG^{commit}" >/dev/null; then
    say "no tag $TAG"
    exit 2
fi

# Checked before building: a build takes minutes on a small machine, and a
# head on another version would make the profiles result mean nothing.
if [ -n "$PROFILES" ]; then
    if [ -z "${REPO:-}" ] || { [ -z "${CLAUDE:-}" ] && [ -z "${OPENCODE:-}" ]; }; then
        say "PROFILES=1 needs REPO and CLAUDE and/or OPENCODE, as make smoke-profiles does"
        exit 2
    fi
    want=${TAG#v}
    have=$(head_version)
    if [ "$have" != "$want" ]; then
        say "smoke-profiles needs the head on $want; the head runs ${have:-nothing that answered}"
        exit 2
    fi
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/pastor-smoke-rc.XXXXXX") || exit 2
wt=$scratch/wt
cleanup() {
    git worktree remove --force "$wt" >/dev/null 2>&1 || true
    git worktree prune >/dev/null 2>&1 || true
    rm -rf "$scratch"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

if ! git worktree add --quiet --detach "$wt" "$TAG" >/dev/null 2>&1; then
    say "could not check out $TAG in a scratch worktree"
    exit 2
fi

host=$(uname -n 2>/dev/null || true)
# The tail of a log with paths and the hostname taken out, fenced.
tail_of() {
    echo
    echo "$1, last $TAIL lines:"
    echo
    echo '```'
    tail -n "$TAIL" "$2" | awk -v wt="$wt" -v home="${HOME:-}" -v host="$host" '
        function swap(s, from, to,   i, out) {
            if (from == "") return s
            out = ""
            while ((i = index(s, from)) > 0) {
                out = out substr(s, 1, i - 1) to
                s = substr(s, i + length(from))
            }
            return out s
        }
        { s = swap($0, wt, "<worktree>"); s = swap(s, home, "~"); print swap(s, host, "<host>") }'
    echo '```'
}

# Runs one make target in the worktree, logging to $scratch/<name>.log.
suite() {
    name=$1
    shift
    say "$name"
    if make -C "$wt" --no-print-directory "$@" >"$scratch/$name.log" 2>&1; then
        echo pass
    else
        echo fail
    fi
}

build=$(suite build build)
smoke="not run"
profiles=skipped
if [ "$build" = pass ]; then
    smoke=$(suite smoke smoke SESSION="$SESSION")
    if [ -n "$PROFILES" ]; then
        profiles=$(suite smoke-profiles smoke-profiles \
            PASTOR="$PASTOR" REPO="$REPO" CLAUDE="${CLAUDE:-}" OPENCODE="${OPENCODE:-}")
    fi
elif [ -n "$PROFILES" ]; then
    profiles="not run"
fi

result=pass
for r in "$build" "$smoke" "$profiles"; do
    [ "$r" = fail ] && result=fail
done
herdr_version=$(herdr --version 2>/dev/null | head -n 1)

echo "### Smoke of $TAG on $LABEL: $result"
echo
echo "- herdr: ${herdr_version:-not found}"
echo
echo "| suite | result |"
echo "|---|---|"
echo "| build | $build |"
echo "| smoke | $smoke |"
echo "| smoke-profiles | $profiles |"
[ "$build" = fail ] && tail_of build "$scratch/build.log"
[ "$smoke" = fail ] && tail_of smoke "$scratch/smoke.log"
[ "$profiles" = fail ] && tail_of smoke-profiles "$scratch/smoke-profiles.log"

[ "$result" = pass ]
