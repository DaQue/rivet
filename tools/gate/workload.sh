#!/usr/bin/env bash
# Workload gate (definition B). See COMPLETION.md.
#
# For every project in projects.tsv: clone it with /usr/bin/git and with rivet, require
# identical object store + index + refs (G1), then run the project's own test command inside
# the rivet clone (G3). G2 (workflow parity) lives in workflow.sh and is run by this script.
#
# Usage: workload.sh [project-name]     (no argument = all projects)
# Env:   RIVET=... GIT=... WORK=... KEEP=1
set -u

REPO=$(cd "$(dirname "$0")/../.." && pwd)
RIVET=${RIVET:-$REPO/target/debug/rivet}
GIT=${GIT:-/usr/bin/git}
WORK=${WORK:-/var/tmp/rivet-gate}
PROJECTS=${PROJECTS:-$REPO/tools/gate/projects.tsv}
KEEP=${KEEP:-0}
ONLY=${1:-}

fail=0
pass=0

say() { printf '%s\n' "$*"; }
ok()  { pass=$((pass + 1)); say "  PASS $*"; }
bad() { fail=$((fail + 1)); say "  FAIL $*"; }

[ -x "$RIVET" ] || { say "rivet binary not executable: $RIVET"; exit 2; }
[ -f "$PROJECTS" ] || { say "no project list: $PROJECTS"; exit 2; }

run_g1() { # name src workdir
  local name=$1 src=$2 dir=$3
  local g=$dir/git r=$dir/rivet

  rm -rf "$g" "$r"
  if ! "$GIT" clone -q "$src" "$g" 2>"$dir/git.clone.err"; then
    bad "$name G1: reference clone failed: $(tail -1 "$dir/git.clone.err")"; return 1
  fi
  if ! "$RIVET" clone -q "$src" "$r" 2>"$dir/rivet.clone.err"; then
    bad "$name G1: rivet clone failed: $(tail -1 "$dir/rivet.clone.err")"; return 1
  fi

  local gh rh gt rt
  gh=$("$GIT" -C "$g" rev-parse HEAD 2>&1) || { bad "$name G1: git HEAD unreadable"; return 1; }
  rh=$("$GIT" -C "$r" rev-parse HEAD 2>&1) || { bad "$name G1: rivet clone HEAD unreadable: $rh"; return 1; }
  [ "$gh" = "$rh" ] && ok "$name G1 HEAD $rh" || bad "$name G1 HEAD: git=$gh rivet=$rh"

  gt=$("$GIT" -C "$g" rev-parse 'HEAD^{tree}')
  rt=$("$GIT" -C "$r" rev-parse 'HEAD^{tree}' 2>&1)
  [ "$gt" = "$rt" ] && ok "$name G1 tree $rt" || bad "$name G1 tree: git=$gt rivet=$rt"

  local gi ri
  gi=$("$GIT" -C "$g" ls-files -s | sha256sum | cut -d' ' -f1)
  ri=$("$GIT" -C "$r" ls-files -s | sha256sum | cut -d' ' -f1)
  [ "$gi" = "$ri" ] && ok "$name G1 index" || bad "$name G1 index: git=$gi rivet=$ri"

  local gr rr
  gr=$("$GIT" -C "$g" for-each-ref --format='%(refname):%(objectname)' | sort | sha256sum | cut -d' ' -f1)
  rr=$("$GIT" -C "$r" for-each-ref --format='%(refname):%(objectname)' | sort | sha256sum | cut -d' ' -f1)
  [ "$gr" = "$rr" ] && ok "$name G1 refs" || bad "$name G1 refs: git=$gr rivet=$rr"

  if "$GIT" -C "$r" fsck --no-progress --no-dangling >"$dir/fsck.log" 2>&1; then
    ok "$name G1 fsck clean"
  else
    sed 's/^/    /' "$dir/fsck.log" | head -5
    bad "$name G1 fsck reported problems"
  fi

  local st
  st=$("$GIT" -C "$r" status --porcelain 2>&1)
  [ -z "$st" ] && ok "$name G1 worktree clean" || bad "$name G1 worktree dirty: $(printf '%s' "$st" | head -3 | tr '\n' ' ')"
  return 0
}

run_g3() { # name rivetclone testcmd seconds
  local name=$1 dir=$2 cmd=$3 secs=$4
  local out=$dir/test.log rc
  ( cd "$dir" && timeout "$secs" bash -c "$cmd" ) >"$out" 2>&1; rc=$?
  if [ $rc -eq 0 ]; then
    ok "$name G3 '$cmd' exit 0"
  else
    say "    last lines of $out:"
    tail -5 "$out" | sed 's/^/    /'
    bad "$name G3 '$cmd' exit $rc"
  fi
}

say "rivet workload gate $(date '+%F %T')  binary=$RIVET"
say "work dir $WORK"
mkdir -p "$WORK"
stamp=$(date +%Y%m%d-%H%M%S)

while IFS=$'\t' read -r name src cmd secs; do
  case "$name" in ''|\#*) continue ;; esac
  [ -n "$ONLY" ] && [ "$name" != "$ONLY" ] && continue
  secs=${secs:-900}
  dir=$WORK/$name-$stamp
  mkdir -p "$dir"
  say ""
  say "== $name  ($src)"
  if run_g1 "$name" "$src" "$dir"; then
    run_g3 "$name" "$dir/rivet" "$cmd" "$secs"
  fi
  [ "$KEEP" = 1 ] || true
done <"$PROJECTS"

say ""
say "== G2 workflow parity"
if bash "$REPO/tools/gate/workflow.sh" "$WORK/workflow-$stamp"; then
  ok "workflow state identical"
else
  bad "workflow state diverged"
fi

say ""
say "gate result: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
