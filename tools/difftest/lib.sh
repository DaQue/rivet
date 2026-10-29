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
# Git needs an editor for `rebase --continue`, `commit --amend`, etc. Without one it aborts with
# "Terminal is dumb, but EDITOR unset" and the scenario compares a git failure against rivet's
# success, which is an artefact of the harness, not a parity difference.
export GIT_EDITOR=: EDITOR=: VISUAL=:
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
    if [ -n "${STOP_ON:-}" ] && [ "$IMPL" = rivet ]; then rm -rf "$base_stop.prev"; cp -a "$PWD" "$base_stop.prev"; fi
    echo "\$ v $*" | sed -e "s#$ROOT#<ROOT>#g" >>"$TRANSCRIPT"
    if [ "$IMPL" = git ]; then
        timeout "${V_TIMEOUT:-60}" "$GIT" "$@" <"${STDIN_FILE:-/dev/null}" >"$TMPOUT" 2>"$TMPERR"; rc=$?
    else
        timeout "${V_TIMEOUT:-60}" "$RIVET" "$@" <"${STDIN_FILE:-/dev/null}" >"$TMPOUT" 2>"$TMPERR"; rc=$?
    fi
    # Normalise the implementation's own name and the per-run root out of the text.
    if [ "${SORT_OUT:-0}" = 1 ]; then sort -o "$TMPOUT" "$TMPOUT"; fi
    # Lines matching DROP_OUT are removed (statistics that legitimately differ, e.g. pack sizes).
    if [ -n "${DROP_OUT:-}" ]; then grep -v -E "$DROP_OUT" "$TMPOUT" >"$TMPOUT.f"; mv "$TMPOUT.f" "$TMPOUT"; fi
    sed -e "s#$ROOT#<ROOT>#g" "$TMPOUT" >>"$TRANSCRIPT"
    if [ -s "$TMPERR" ] && [ "${NO_ERR:-0}" != 1 ]; then
        echo "--- stderr" >>"$TRANSCRIPT"
        sed -e "s#$ROOT#<ROOT>#g" "$TMPERR" >>"$TRANSCRIPT"
    fi
    echo "[exit $rc]" >>"$TRANSCRIPT"
    # Debugging aid: freeze the scenario (and its directories) at the first matching error.
    if [ -n "${STOP_ON:-}" ] && [ "$IMPL" = rivet ] && grep -q "$STOP_ON" "$TMPERR"; then
        echo "STOPPED in $PWD" >>"$base_stop"; exit 77
    fi
    return $rc
}

g() { "$GIT" "$@"; }

# Like g, but the command, its output and its exit code go into the transcript, so what real git
# reads out of a repository that rivet just edited is compared between the two runs.
vg() {
    local rc
    echo "\$ g $*" | sed -e "s#$ROOT#<ROOT>#g" >>"$TRANSCRIPT"
    "$GIT" "$@" <"${STDIN_FILE:-/dev/null}" >"$TMPOUT" 2>"$TMPERR"; rc=$?
    sed -e "s#$ROOT#<ROOT>#g" "$TMPOUT" >>"$TRANSCRIPT"
    if [ -s "$TMPERR" ]; then
        echo "--- stderr" >>"$TRANSCRIPT"
        sed -e "s#$ROOT#<ROOT>#g" "$TMPERR" >>"$TRANSCRIPT"
    fi
    echo "[exit $rc]" >>"$TRANSCRIPT"
    return $rc
}

# Like v, but with the output lines sorted (for commands whose order is a hash-table accident).
# Like v, but the stderr text is not compared (only stdout and the exit code), for commands whose
# diagnostics are free-form.
vq() { NO_ERR=1 v "$@"; local rc=$?; NO_ERR=0; return $rc; }

# Like v, with the given text on standard input.
vin() { local text=$1; shift; STDIN_FILE=$(mktemp "${TMPDIR:-/tmp}/stdin.XXXXXX"); printf '%s' "$text" >"$STDIN_FILE"; v "$@"; local rc=$?; rm -f "$STDIN_FILE"; STDIN_FILE=; return $rc; }

vs() { SORT_OUT=1 v "$@"; local rc=$?; SORT_OUT=0; return $rc; }

# Repository state as real git sees it, for one repository directory.
state_of() {
    local dir=$1
    (
        cd "$dir" || exit
        echo "## HEAD"; cat "$(g rev-parse --git-dir)/HEAD" 2>&1
        echo "## refs"; g for-each-ref --format='%(refname) %(objectname) %(symref)' 2>&1
        echo "## index"; g ls-files -s 2>&1
        echo "## status"; g status --porcelain=v1 --untracked-files=all 2>&1
        echo "## worktree"; find . -path ./.git -prune -o -path ./objects -prune -o -path ./hooks -prune -o -path ./info/refs -prune -o -type f -print | sort | while read -r f; do if [ "$f" = ./config ]; then h=$(sed "s#$ROOT#<ROOT>#g" "$f" | sha1sum | cut -c1-12); else h=$(sha1sum <"$f" | cut -c1-12); fi; echo "$f $h $(stat -c %a "$f")"; done
        local gd; gd=$(g rev-parse --git-dir)
        for log in $(cd "$gd" && find logs -type f 2>/dev/null | sort); do
            echo "## $log"; cut -d' ' -f1,2 "$gd/$log"; cut -f2 "$gd/$log"
        done
        for f in ORIG_HEAD MERGE_HEAD MERGE_MSG MERGE_MODE SQUASH_MSG CHERRY_PICK_HEAD REVERT_HEAD FETCH_HEAD packed-refs config; do
            [ -f "$gd/$f" ] && { echo "## $f"; cat "$gd/$f"; }
        done
        [ -f "$gd/MERGE_RR" ] && { echo "## MERGE_RR"; tr '\0' '@' <"$gd/MERGE_RR"; echo; }
        if [ -d "$gd/rr-cache" ]; then
            echo "## rr-cache"
            (cd "$gd" && find rr-cache -type f | sort | while read -r f; do echo "$f $(sha1sum <"$f" | cut -c1-12)"; done)
        fi
        echo "## fsck"; g fsck --strict --no-progress 2>&1 | grep -v '^dangling tree' | sort; echo "fsck exit ${PIPESTATUS[0]}"
    )
}

run_scenario() {
    local name=$1; shift
    if ! declare -F "$name" >/dev/null; then
        echo "FAIL $name  (no such scenario)"
        FAILURES=$((FAILURES + 1))
        return 1
    fi
    local base; base=$(mktemp -d "${TMPDIR:-/tmp}/difftest.XXXXXX")
    TMPOUT=$base/out TMPERR=$base/err; base_stop=$base/stopped
    for IMPL in git rivet; do
        ROOT=$base/$IMPL
        mkdir -p "$ROOT"
        export GIT_CONFIG_GLOBAL=$ROOT.gitconfig
        # Git's own post-command maintenance (it can repack, write a multi-pack-index or a cruft pack in
        # the middle of a scenario) is switched off: the scenarios drive every repack themselves.
        printf '[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = true\n[maintenance]\n\tauto = false\n[gc]\n\tauto = 0\n' >"$GIT_CONFIG_GLOBAL"
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
        [ -n "${KEEP:-}" ] || rm -rf "$base"
    else
        echo "FAIL $name  ($base)"
        FAILURES=$((FAILURES + 1))
        if [ "${VERBOSE:-0}" = 1 ]; then cat "$base/transcript.diff" "$base/state.diff"; fi
    fi
}
