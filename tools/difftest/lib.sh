#!/usr/bin/env bash
# Differential test harness: run one scenario with real git and with rivet, then compare the
# transcripts (stdout, stderr, exit codes) and the resulting repository state.
#
# A scenario is a bash function that uses `v <args>` wherever it would call git, and plain
# shell for file edits. `g <args>` always calls real git (for setup that is not under test).
# Usage: source lib.sh; run_scenario <function> [compare-dir...]

set -u
RIVET=${RIVET:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/debug/rivet}
GIT=${GIT:-/usr/bin/git}

export GIT_AUTHOR_NAME="A U Thor" GIT_AUTHOR_EMAIL="author@example.com"
export GIT_COMMITTER_NAME="C O Mitter" GIT_COMMITTER_EMAIL="committer@example.com"
export GIT_AUTHOR_DATE="1700000000 +0100" GIT_COMMITTER_DATE="1700000000 +0100"
export GIT_CONFIG_NOSYSTEM=1 TZ=UTC LC_ALL=C GIT_PAGER=cat PAGER=cat
unset GIT_DIR GIT_WORK_TREE

FAILURES=0

# Advance the fake clock so successive commits get distinct, deterministic dates.
tick() {
    local now=${GIT_COMMITTER_DATE%% *}
    now=$((now + 60))
    export GIT_AUTHOR_DATE="$now +0100" GIT_COMMITTER_DATE="$now +0100"
}

v() {
    local out rc
    echo "\$ v $*" >>"$TRANSCRIPT"
    if [ "$IMPL" = git ]; then
        "$GIT" "$@" >"$TMPOUT" 2>"$TMPERR"; rc=$?
    else
        "$RIVET" "$@" >"$TMPOUT" 2>"$TMPERR"; rc=$?
    fi
    # Normalise the implementation's own name and the per-run root out of the text.
    sed -e "s#$ROOT#<ROOT>#g" "$TMPOUT" >>"$TRANSCRIPT"
    if [ -s "$TMPERR" ]; then
        echo "--- stderr" >>"$TRANSCRIPT"
        sed -e "s#$ROOT#<ROOT>#g" "$TMPERR" >>"$TRANSCRIPT"
    fi
    echo "[exit $rc]" >>"$TRANSCRIPT"
    return $rc
}

g() { "$GIT" "$@"; }

# Repository state as real git sees it, for one repository directory.
state_of() {
    local dir=$1
    (
        cd "$dir" || exit
        echo "## HEAD"; cat "$(g rev-parse --git-dir)/HEAD" 2>&1
        echo "## refs"; g for-each-ref --format='%(refname) %(objectname) %(symref)' 2>&1
        echo "## index"; g ls-files -s 2>&1
        echo "## status"; g status --porcelain=v1 --untracked-files=all 2>&1
        echo "## worktree"; find . -path ./.git -prune -o -type f -print | sort | while read -r f; do echo "$f $(sha1sum <"$f" | cut -c1-12) $(stat -c %a "$f")"; done
        local gd; gd=$(g rev-parse --git-dir)
        for log in $(cd "$gd" && find logs -type f 2>/dev/null | sort); do
            echo "## $log"; cut -d' ' -f1,2 "$gd/$log"; cut -f2 "$gd/$log"
        done
        for f in ORIG_HEAD MERGE_HEAD MERGE_MSG MERGE_MODE FETCH_HEAD packed-refs config; do
            [ -f "$gd/$f" ] && { echo "## $f"; cat "$gd/$f"; }
        done
        echo "## fsck"; g fsck --strict --no-progress 2>&1 | grep -v '^dangling tree'; echo "fsck exit $?"
    )
}

run_scenario() {
    local name=$1; shift
    local base; base=$(mktemp -d "${TMPDIR:-/tmp}/difftest.XXXXXX")
    TMPOUT=$base/out TMPERR=$base/err
    for IMPL in git rivet; do
        ROOT=$base/$IMPL
        mkdir -p "$ROOT"
        export GIT_CONFIG_GLOBAL=$ROOT.gitconfig
        printf '[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = true\n' >"$GIT_CONFIG_GLOBAL"
        TRANSCRIPT=$base/$IMPL.transcript
        : >"$TRANSCRIPT"
        export GIT_AUTHOR_DATE="1700000000 +0100" GIT_COMMITTER_DATE="1700000000 +0100"
        ( cd "$ROOT" && "$name" ) >/dev/null 2>&1
        : >"$base/$IMPL.state"
        for dir in "${@:-repo}"; do
            echo "#### $dir" >>"$base/$IMPL.state"
            [ -d "$ROOT/$dir" ] && state_of "$ROOT/$dir" | sed -e "s#$ROOT#<ROOT>#g" >>"$base/$IMPL.state"
        done
    done
    local ok=1
    if ! diff -u "$base/git.transcript" "$base/rivet.transcript" >"$base/transcript.diff"; then ok=0; fi
    if ! diff -u "$base/git.state" "$base/rivet.state" >"$base/state.diff"; then ok=0; fi
    if [ $ok = 1 ]; then
        echo "PASS $name"
        rm -rf "$base"
    else
        echo "FAIL $name  ($base)"
        FAILURES=$((FAILURES + 1))
        if [ "${VERBOSE:-0}" = 1 ]; then cat "$base/transcript.diff" "$base/state.diff"; fi
    fi
}
