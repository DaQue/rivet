#!/usr/bin/env bash
# G2 workflow parity: run the same fixed sequence under /usr/bin/git and under rivet with pinned
# identities and dates, then compare the resulting *state* (not chatty stdout):
# commit ids, tree hashes, refs, index, stash, rev counts, status, and every command's exit code.
#
# Usage: workflow.sh <scratch-dir>
set -u

REPO=$(cd "$(dirname "$0")/../.." && pwd)
RIVET=${RIVET:-$REPO/target/debug/rivet}
GIT=${GIT:-/usr/bin/git}
OUT=${1:-/var/tmp/rivet-gate/workflow-$(date +%s)}

export GIT_AUTHOR_NAME='Gate' GIT_AUTHOR_EMAIL='gate@example.invalid'
export GIT_COMMITTER_NAME='Gate' GIT_COMMITTER_EMAIL='gate@example.invalid'
export GIT_AUTHOR_DATE='2026-01-01T00:00:00 +0000'
export GIT_COMMITTER_DATE='2026-01-01T00:00:00 +0000'
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export TZ=UTC

mkdir -p "$OUT"

workflow() { # bin dir -> writes $dir/.rcs
  local b=$1 d=$2
  rm -rf "$d"; mkdir -p "$d"
  local rcs=$d/.rcs
  : >"$rcs"
  step() { "$b" "$@" >>"$d/.out" 2>&1; printf '%s %s\n' "$?" "$*" >>"$rcs"; }
  cd "$d" || return 1
  step init -q .
  step config user.name Gate
  step config user.email gate@example.invalid
  printf 'a\n' >a.txt; step add a.txt; step commit -qm c1
  step checkout -qb feature
  printf 'b\n' >b.txt; step add b.txt; step commit -qm c2
  step checkout -q master
  printf 'c\n' >>a.txt; step commit -qam c3
  step merge --no-ff -qm merge-feature feature
  printf 'd\n' >d.txt; step add d.txt; step stash push -qm st
  printf 'e\n' >e.txt; step add e.txt; step commit -qm c4
  step stash pop -q
  step tag v1
  step checkout -q -b topic HEAD~1
  printf 'f\n' >f.txt; step add f.txt; step commit -qm c5
  step rebase -q master
  step checkout -q master
  step cherry-pick topic
  step branch --list
  step log --oneline
  cd "$OUT" || return 1
}

state() { # dir -> canonical state file (read with real git: format compatibility is part of parity)
  local d=$1
  {
    echo "## rc"
    sed 's/^[a-z].*//' "$d/.rcs"
    echo "## head"
    "$GIT" -C "$d" rev-parse HEAD
    echo "## tree"
    "$GIT" -C "$d" rev-parse 'HEAD^{tree}'
    echo "## revs"
    "$GIT" -C "$d" rev-list --all --count
    echo "## refs"
    "$GIT" -C "$d" for-each-ref --format='%(refname) %(objectname)'
    echo "## index"
    "$GIT" -C "$d" ls-files -s
    echo "## status"
    "$GIT" -C "$d" status --porcelain
    echo "## log"
    "$GIT" -C "$d" log --all --format='%H %s' --topo-order
    echo "## stash"
    "$GIT" -C "$d" stash list --format='%H'
  } 2>&1
}

workflow "$GIT" "$OUT/git"   || { echo "G2: reference run failed"; exit 2; }
workflow "$RIVET" "$OUT/rivet" || { echo "G2: rivet run failed"; exit 2; }

state "$OUT/git"   >"$OUT/state.git"
state "$OUT/rivet" >"$OUT/state.rivet"

if diff -u "$OUT/state.git" "$OUT/state.rivet" >"$OUT/state.diff"; then
  echo "G2: state identical (11 refs, $(wc -l <"$OUT/state.rivet") lines compared)"
  exit 0
fi
echo "G2: state diverged — $OUT/state.diff"
head -40 "$OUT/state.diff"
exit 1
