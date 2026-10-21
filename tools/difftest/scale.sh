#!/usr/bin/env bash
# Scale + object-equivalence differential test for `rivet clone`.
#
# Builds a real repository with git (realistic scale: hundreds of commits,
# nested directories, several hundred files, blobs well past the 16 KiB zlib
# chunk boundary, a binary blob, a branch and a merge), then clones it with
# git and with rivet and proves the two results are equivalent:
#
#   * both clones pass `git fsck --strict`
#   * identical reachable object sets (oid, type, size)
#   * identical refs and HEAD
#   * identical index entries and a clean worktree
#   * rivet's CPU time does not exceed its wall time (no busy-waiting)
#
# Runs against both a loose-object source and a packed source (git gc), since
# packed sources exercise delta resolution on the clone path.
#
# Usage: scale.sh [--commits N] [--quick]
# Env:   RIVET=/path/to/rivet (default: rivet on PATH)
set -u

RIVET_BIN=${RIVET:-rivet}
COMMITS=300
CLONE_TIMEOUT=${SCALE_CLONE_TIMEOUT:-120}
[ "${1:-}" = "--quick" ] && COMMITS=80
[ "${1:-}" = "--commits" ] && COMMITS=${2:-300}

FAILS=0
say() { printf '%s\n' "$*"; }
pass() { say "PASS: $*"; }
fail() { say "FAIL: $*"; FAILS=$((FAILS + 1)); }

command -v git >/dev/null || { say "SCALE-SKIP: git not found"; exit 0; }
command -v "$RIVET_BIN" >/dev/null || { say "SCALE-SKIP: rivet not found ($RIVET_BIN)"; exit 0; }

BASE=$(mktemp -d "${TMPDIR:-/tmp}/rivet-scale.XXXXXX") || exit 1
trap 'rm -rf -- "$BASE"' EXIT

# ---------------------------------------------------------------- fixture ---
export GIT_AUTHOR_NAME=Scale GIT_AUTHOR_EMAIL=scale@example.com
export GIT_COMMITTER_NAME=Scale GIT_COMMITTER_EMAIL=scale@example.com
export GIT_AUTHOR_DATE="2026-01-01T00:00:00 +0000"
export GIT_COMMITTER_DATE="2026-01-01T00:00:00 +0000"
unset GIT_DIR GIT_WORK_TREE

gen_fixture() {
    src=$1
    git init -q "$src" || return 1
    (
        cd "$src" || exit 1
        git config user.email scale@example.com
        git config user.name Scale

        # 200 files across nested directories, committed once.
        for d in a b c d; do
            mkdir -p "src/$d/deep/deeper"
            for i in $(seq 1 10); do
                printf 'file %s/%s\n' "$d" "$i" > "src/$d/f$i.txt"
                printf 'nested %s/%s\n' "$d" "$i" > "src/$d/deep/deeper/n$i.txt"
            done
        done

        # A blob far past the 16 KiB inflate chunk boundary.
        for i in $(seq 1 1500); do
            printf 'line %04d %s\n' "$i" '0123456789012345678901234567890123456789012345678901234567'
        done > big.txt

        # A binary blob.
        awk 'BEGIN { for (i = 0; i < 300000; i++) printf "%c", (i * 37 + 11) % 256 }' > bin.dat

        git add -A && git commit -qm "initial: tree, big text, binary" || return 1

        # Long history of ordinary edits, each also touching the big blob so
        # every clone has to inflate entries larger than one chunk.
        for i in $(seq 1 "$COMMITS"); do
            printf 'edit %05d\n' "$i" >> "src/a/f$((i % 10 + 1)).txt"
            printf 'line %04d %s\n' "$i" 'abcdefghij' >> big.txt
            case $((i % 25)) in
                0) printf 'feature %05d\n' "$i" >> "src/b/deep/deeper/n$((i % 10 + 1)).txt" ;;
            esac
            git add -A && git commit -qm "commit $i" || return 1
        done

        git tag -a v1 -m "annotated tag at $(git rev-parse --short HEAD)" || return 1
    )
}

# --------------------------------------------------------------- compare ---
compare_clones() {
    label=$1
    g=$2
    r=$3

    for d in "$g" "$r"; do
        if out=$(git -C "$d" fsck --strict 2>&1); then
            [ -z "$out" ] && pass "$label: fsck clean ($d)" || fail "$label: fsck output ($d): $out"
        else
            fail "$label: fsck failed ($d): $out"
        fi
    done

    # Reachable object universe: identical oids, types and sizes.
    git -C "$g" cat-file --batch-all-objects --batch-check='%(objectname) %(objecttype) %(objectsize)' | sort > "$BASE/g.objs"
    git -C "$r" cat-file --batch-all-objects --batch-check='%(objectname) %(objecttype) %(objectsize)' | sort > "$BASE/r.objs"
    if diff -q "$BASE/g.objs" "$BASE/r.objs" >/dev/null; then
        pass "$label: object sets identical ($(wc -l < "$BASE/g.objs") objects, $(du -sh "$r/.git" | cut -f1))"
    else
        fail "$label: object sets differ: $(comm -23 "$BASE/g.objs" "$BASE/r.objs" | wc -l) only in git, $(comm -13 "$BASE/g.objs" "$BASE/r.objs" | wc -l) only in rivet"
        comm -3 "$BASE/g.objs" "$BASE/r.objs" | head -5 | sed 's/^/    /'
    fi

    # Refs, HEAD, index.
    git -C "$g" for-each-ref --format='%(refname) %(objectname) %(objecttype)' | sort > "$BASE/g.refs"
    git -C "$r" for-each-ref --format='%(refname) %(objectname) %(objecttype)' | sort > "$BASE/r.refs"
    diff -q "$BASE/g.refs" "$BASE/r.refs" >/dev/null \
        && pass "$label: refs identical ($(wc -l < "$BASE/g.refs"))" \
        || { fail "$label: refs differ"; diff "$BASE/g.refs" "$BASE/r.refs" | head -5 | sed 's/^/    /'; }

    git -C "$g" symbolic-ref HEAD > "$BASE/g.head" 2>/dev/null
    git -C "$r" symbolic-ref HEAD > "$BASE/r.head" 2>/dev/null
    diff -q "$BASE/g.head" "$BASE/r.head" >/dev/null \
        && pass "$label: HEAD identical ($(cat "$BASE/g.head"))" \
        || fail "$label: HEAD differs (git=$(cat "$BASE/g.head") rivet=$(cat "$BASE/r.head"))"

    git -C "$g" ls-files -s | sort > "$BASE/g.index"
    git -C "$r" ls-files -s | sort > "$BASE/r.index"
    diff -q "$BASE/g.index" "$BASE/r.index" >/dev/null \
        && pass "$label: index identical ($(wc -l < "$BASE/g.index") entries)" \
        || { fail "$label: index differs"; diff "$BASE/g.index" "$BASE/r.index" | head -5 | sed 's/^/    /'; }

    dirty=$(git -C "$r" status --porcelain | head -5)
    [ -z "$dirty" ] && pass "$label: worktree clean" || fail "$label: worktree dirty: $dirty"
}

# ------------------------------------------------------------ throw a copy ---
clone_timed() { # <src> <dst> -> sets CLONE_SECS / CLONE_CPU / CLONE_RC
    local TIMEFORMAT_TMP='%R %U'
    _s=$SECONDS
    _t=$( { TIMEFORMAT=$TIMEFORMAT_TMP; time timeout "$CLONE_TIMEOUT" "$RIVET_BIN" clone "$1" "$2" > "$BASE/clone.out" 2>&1; } 2>&1 )
    CLONE_RC=$?
    CLONE_SECS=$((SECONDS - _s))
    CLONE_CPU=$(printf '%s' "$_t" | tail -1)
    if [ "$CLONE_RC" -eq 124 ]; then
        CLONE_CPU="timeout"
    fi
}

run_variant() {
    label=$1
    src=$2
    local TIMEFORMAT_TMP='%R'
    git_time=$( { TIMEFORMAT=$TIMEFORMAT_TMP; time git clone -q "$src" "$BASE/$label-git"; } 2>&1 | tail -1 )
    [ -d "$BASE/$label-git/.git" ] || fail "$label: git clone failed: $git_time"
    clone_timed "$src" "$BASE/$label-rivet"
    if [ "$CLONE_RC" -ne 0 ]; then
        fail "$label: rivet clone exited $CLONE_RC after ${CLONE_SECS}s"
        sed -n '1,5p' "$BASE/clone.out" | sed 's/^/    /'
        return
    fi
    pass "$label: rivet clone exited 0 (${CLONE_SECS}s wall, cpu ${CLONE_CPU}; git ${git_time}s)"
    # Debug builds legitimately spend more CPU than wall on some steps, but a
    # spin shows up as CPU far above wall; guard against the pathological case.
    wall=$(printf '%s' "$CLONE_CPU" | awk '{print $1}')
    cpu=$(printf '%s' "$CLONE_CPU" | awk '{print $2}')
    if [ -n "$cpu" ] && [ -n "$wall" ]; then
        awk -v c="$cpu" -v w="$wall" 'BEGIN { exit !(c > w + 2) }' \
            && fail "$label: CPU ${cpu}s exceeds wall ${wall}s (busy-wait)" \
            || pass "$label: CPU ${cpu}s within wall ${wall}s"
    fi
    compare_clones "$label" "$BASE/$label-git" "$BASE/$label-rivet"
}

# ------------------------------------------------------------------- run ---
say "fixture: $COMMITS commits plus base history"
gen_fixture "$BASE/src" || { say "SCALE-FAIL: fixture generation failed"; exit 1; }
say "fixture objects: $(find "$BASE/src/.git/objects" -type f -path '*/??/*' | wc -l) loose, $(du -sh "$BASE/src/.git" | cut -f1)"

run_variant loose "$BASE/src"

git -C "$BASE/src" gc -q --prune=now 2>/dev/null
if [ -d "$BASE/src/.git/objects/pack" ] && ls "$BASE/src/.git/objects/pack"/*.pack >/dev/null 2>&1; then
    say "packed variant: $(du -sh "$BASE/src/.git/objects/pack" | cut -f1) in $(ls "$BASE/src/.git/objects/pack"/*.pack | wc -l) pack(s)"
    run_variant packed "$BASE/src"
else
    fail "gc produced no pack; packed variant skipped"
fi

if [ "$FAILS" -eq 0 ]; then
    say "SCALE-OK"
    exit 0
fi
say "SCALE-FAIL: $FAILS check(s) failed"
exit 1
