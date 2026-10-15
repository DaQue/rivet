#!/bin/bash
# qwen-batch.sh [--max-hours N] [--no-edit-abort M] <task-name> [<task-name> ...]
#
# UNATTENDED queue runner for the local-qwen implementation harness.
# One launch (un-gated, no prompts inside), then it walks the queue by itself:
#
#   preflight (shim on :11499 + no other run) -> per task:
#     thermal guard -> qwen-impl.fish -> verdict from the TREE (cargo check + cargo test)
#     -> commit iff green -> next task
#
# It stops on: queue empty, deadline (--max-hours, default 4), or a failed preflight.
# It does NOT stop on a failing task -- a red task is logged and the queue moves on, so
# one bad brief cannot cost the other three hours.
#
# NO-EDIT TRIPWIRE (--no-edit-abort M, default 12 minutes):
#   An observed failure mode is the model looping on Read and never editing anything:
#   a 120-turn run that made 96 Read calls, 0 Edit calls, and burned 93 minutes without
#   writing a byte (2026-10-15). Prompt-level rules do not stop that. So this is an
#   out-of-band gate: M minutes in, if the run log has no Edit event at all, the harness's
#   whole process group is killed. The task goes red, the queue moves on, and a spin-out
#   costs 12 minutes instead of the whole wall clock.
#
# Durable record (one line per event; read this, not the 4-hour stream log):
#   $HOME/.hermes/cache/scratch/qwen-impl/BATCH-<stamp>.status
# Per-task artifacts in the same dir:
#   <task>.guard-<stamp>.log   guard + harness stdout (the stream)
#   <task>.run-<stamp>.log     harness NDJSON event stream (Read/Edit/Bash counts live here)
#   <task>.thermal-<stamp>.log guard samples
#   <task>.check-<stamp>.log   cargo check --workspace --all-targets
#   <task>.test-<stamp>.log    cargo test -p rivet-core
#
# Launch the /tmp copy (the repo copy here is the readable original):
#   cp tools/qwen-batch.sh /tmp/rivet-batch.sh
#   systemd-run --user --unit=qwen-batch --collect --wait bash /tmp/rivet-batch.sh T03d2a T03d2b
set -u
MAX_HOURS=${MAX_HOURS:-4}
NOEDIT_MINS=${NOEDIT_MINS:-12}
while [ "${1:-}" = "--max-hours" ]; do MAX_HOURS=${2:-4}; shift 2; done
while [ "${1:-}" = "--no-edit-abort" ]; do NOEDIT_MINS=${2:-12}; shift 2; done
if [ $# -lt 1 ]; then echo "usage: qwen-batch.sh [--max-hours N] [--no-edit-abort M] <task-name>..."; exit 2; fi

ROOT=/home/davidq/Projects/rivet
OUT=$HOME/.hermes/cache/scratch/qwen-impl
GUARD=/tmp/rivet-guard.sh
STAMP=$(date +%Y%m%d-%H%M%S)
STATUS=$OUT/BATCH-$STAMP.status
DEADLINE=$(( $(date +%s) + MAX_HOURS * 3600 ))
mkdir -p "$OUT" || exit 1
cd "$ROOT" || exit 1

log() { printf '%s %-9s %s\n' "$(date +%H:%M:%S)" "$1" "${*:2}" | tee -a "$STATUS"; }
stubs() { grep -r 'todo!()' crates --include='*.rs' 2>/dev/null | wc -l; }
# newest run log for a task that was NOT there before this task started
newrl() {
  for f in "$OUT/$1.run-"*.log; do
    [ -f "$f" ] || continue
    case "$2" in *"$f"*) ;; *) echo "$f"; return ;; esac
  done
}

log BATCH "start max=${MAX_HOURS}h noedit=${NOEDIT_MINS}m tasks=$*"

# ---- preflight: the only failure mode that would poison every task in the queue ----
[ -f "$GUARD" ] || cp tools/qwen-thermal-guard.sh "$GUARD" 2>/dev/null
if ! ss -ltn 2>/dev/null | grep -q ':11499'; then
  log PREFLIGHT "shim down on :11499 -- starting it"
  fish -lc 'qwen-shim-start' >/dev/null 2>&1
  for _ in $(seq 1 30); do ss -ltn 2>/dev/null | grep -q ':11499' && break; sleep 2; done
fi
if ! ss -ltn 2>/dev/null | grep -q ':11499'; then
  log PREFLIGHT "ABORT: shim never came up on :11499 -- no task launched"
  exit 3
fi
if pgrep -f 'qwen-impl.fish' >/dev/null 2>&1; then
  log PREFLIGHT "ABORT: another qwen run is already active (shared GPU + target/)"
  exit 4
fi
[ -x "$GUARD" ] || { log PREFLIGHT "ABORT: no thermal guard at $GUARD"; exit 5; }
log PREFLIGHT "ok: shim up, guard $GUARD, stubs=$(stubs)"

for t in "$@"; do
  if [ "$(date +%s)" -ge "$DEADLINE" ]; then log BATCH "deadline (${MAX_HOURS}h) reached before $t -- stopping"; break; fi
  tf="tools/tasks/$t.md"
  if [ ! -f "$tf" ]; then log "$t" "SKIP: no $tf"; continue; fi

  before=$(stubs); head_before=$(git rev-parse --short HEAD); t0=$(date +%s)
  pre_rl=$(ls "$OUT/$t.run-"*.log 2>/dev/null)
  log "$t" "START stubs=$before head=$head_before"

  # own process group so the harness (setsid'd inside the guard) can be killed as a unit
  setsid bash "$GUARD" "$tf" "$t" > "$OUT/$t.guard-$STAMP.log" 2>&1 &
  gpid=$!

  # --- out-of-band no-Edit tripwire: the model cannot talk its way past this one ---
  (
    el=0
    while [ "$el" -lt "$NOEDIT_MINS" ]; do
      kill -0 "$gpid" 2>/dev/null || exit 0
      sleep 60; el=$((el+1))
    done
    kill -0 "$gpid" 2>/dev/null || exit 0
    rl=$(newrl "$t" "$pre_rl")
    if [ -z "$rl" ] || ! grep -q '"name":"Edit"' "$rl" 2>/dev/null; then
      nr=$(grep -c '"name":"Read"' "$rl" 2>/dev/null || echo 0)
      printf '%s %-9s %s\n' "$(date +%H:%M:%S)" "$t" \
        "NO-EDIT-ABORT ${NOEDIT_MINS}m, 0 Edit events, ${nr} Reads -- killing harness" >> "$STATUS"
      hpid=$(pgrep -f 'tools/qwen-impl.fish' | head -1)
      if [ -n "${hpid:-}" ]; then
        hpgid=$(ps -o pgid= -p "$hpid" 2>/dev/null | tr -d ' ')
        [ -n "$hpgid" ] && kill -TERM -"$hpgid" 2>/dev/null
        sleep 5
        [ -n "$hpgid" ] && kill -KILL -"$hpgid" 2>/dev/null
      fi
      kill -TERM "$gpid" 2>/dev/null
    fi
  ) &
  trip=$!

  wait "$gpid"; gst=$?
  kill "$trip" 2>/dev/null; wait "$trip" 2>/dev/null
  mins=$(( ($(date +%s) - t0) / 60 ))
  after=$(stubs)
  rl=$(newrl "$t" "$pre_rl")
  edits=$(grep -c '"name":"Edit"' "$rl" 2>/dev/null || true); edits=${edits:-0}

  # Verdict from the tree, never from the run: the guard's exit code and the model's own
  # summary are both produced by the process under test and distinguish nothing.
  cargo check --workspace --all-targets --message-format=short > "$OUT/$t.check-$STAMP.log" 2>&1
  cst=$?
  cargo test -p rivet-core > "$OUT/$t.test-$STAMP.log" 2>&1
  tst=$?
  files=$(git status --short | awk '{print $2}' | tr '\n' ' ')

  if [ "$cst" -eq 0 ] && [ "$tst" -eq 0 ] && [ "$after" -lt "$before" ]; then
    if git add -A -- crates docs tools >/dev/null 2>&1 && ! git diff --cached --quiet; then
      sha=$(git commit -q -m "$t: qwen batch (check+test green, stubs $before->$after)" && git rev-parse --short HEAD)
      log "$t" "GREEN commit=${sha:-commit-failed} check=0 test=0 stubs=$before->$after edits=$edits ${mins}m files=[$files]"
    else
      log "$t" "GREEN-no-diff check=0 test=0 stubs=$before->$after edits=$edits ${mins}m"
    fi
  else
    log "$t" "FAIL check=$cst test=$tst stubs=$before->$after edits=$edits ${mins}m guard=$gst files=[$files]"
  fi
done

log BATCH "end stubs=$(stubs) head=$(git rev-parse --short HEAD)"
echo "=== status: $STATUS ==="
tail -40 "$STATUS"
