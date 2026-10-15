#!/bin/bash
# qwen-batch.sh [--max-hours N] <task-name> [<task-name> ...]
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
# Durable record (one line per event; read this, not the 4-hour stream log):
#   $HOME/.hermes/cache/scratch/qwen-impl/BATCH-<stamp>.status
# Per-task artifacts in the same dir:
#   <task>.guard-<stamp>.log   guard + harness stdout (the stream)
#   <task>.check-<stamp>.log   cargo check --workspace --all-targets
#   <task>.test-<stamp>.log    cargo test -p rivet-core
#
# Launch the /tmp copy (the repo copy here is the readable original):
#   cp tools/qwen-batch.sh /tmp/rivet-batch.sh
#   systemd-run --user --unit=qwen-batch --collect --wait bash /tmp/rivet-batch.sh T03d2 T03e T04
set -u
MAX_HOURS=${MAX_HOURS:-4}
while [ "${1:-}" = "--max-hours" ]; do MAX_HOURS=${2:-4}; shift 2; done
if [ $# -lt 1 ]; then echo "usage: qwen-batch.sh [--max-hours N] <task-name>..."; exit 2; fi

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

log BATCH "start max=${MAX_HOURS}h tasks=$*"

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
  log "$t" "START stubs=$before head=$head_before"
  bash "$GUARD" "$tf" "$t" > "$OUT/$t.guard-$STAMP.log" 2>&1
  gst=$?
  mins=$(( ($(date +%s) - t0) / 60 ))
  after=$(stubs)

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
      log "$t" "GREEN commit=${sha:-commit-failed} check=0 test=0 stubs=$before->$after ${mins}m files=[$files]"
    else
      log "$t" "GREEN-no-diff check=0 test=0 stubs=$before->$after ${mins}m"
    fi
  else
    log "$t" "FAIL check=$cst test=$tst stubs=$before->$after ${mins}m guard=$gst files=[$files]"
  fi
done

log BATCH "end stubs=$(stubs) head=$(git rev-parse --short HEAD)"
echo "=== status: $STATUS ==="
tail -40 "$STATUS"
