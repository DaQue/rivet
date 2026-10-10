#!/bin/bash
# qwen-thermal-guard.sh <task-file> [task-name]
#
# Runs the qwen implementation harness (tools/qwen-impl.fish) for ONE task while
# sampling CPU/GPU temperatures every 30s. Kills the harness's whole process group
# if a KILL tier is crossed (qwen hits every core + the GPU, so a stalled or looping
# model can cook the machine); logs WARN crossings without touching the run.
#
# Tiers (user instruction, 2026-10-10):
#   WARN  cpu_tctl 78 | gpu_edge 85 | gpu_junction 95 | gpu_mem 92
#   KILL  cpu_tctl 86 | gpu_edge 92 | gpu_junction 105 | gpu_mem 100
#
# Output: $HOME/.hermes/cache/scratch/qwen-impl/<name>.run-<stamp>.log  (harness stdout)
#         $HOME/.hermes/cache/scratch/qwen-impl/<name>.thermal-<stamp>.log (samples)
set -u
TASK=${1:?usage: qwen-thermal-guard.sh <task-file> [name]}
NAME=${2:-$(basename "$TASK" .md)}
STAMP=$(date +%Y%m%d-%H%M%S)
OUT=$HOME/.hermes/cache/scratch/qwen-impl
mkdir -p "$OUT"
RUNLOG=$OUT/$NAME.run-$STAMP.log
TLOG=$OUT/$NAME.thermal-$STAMP.log
cd /home/davidq/Projects/rivet || exit 1

setsid fish tools/qwen-impl.fish "$TASK" "$NAME" > "$RUNLOG" 2>&1 &
HPID=$!
PGID=$(ps -o pgid= -p "$HPID" | tr -d ' ')
echo "guard: harness pid=$HPID pgid=$PGID"
echo "guard: run-log   $RUNLOG"
echo "guard: thermal   $TLOG"

# max reading for one sensor label across all chips (amdgpu reports two 'edge' lines)
temp() {
  sensors 2>/dev/null | awk -v want="$1" '
    index($1, want":") == 1 { v=$2; gsub(/[+C\xc2\xb0]/,"",v); if (v+0 > m) m = v+0 }
    END { printf "%d", m+0 }'
}

KILLED=0
while kill -0 "$HPID" 2>/dev/null; do
  CT=$(temp Tctl); GE=$(temp edge); GJ=$(temp junction); GM=$(temp mem)
  printf '%s cpu_tctl=%-4s gpu_edge=%-4s gpu_junction=%-4s gpu_mem=%-4s\n' \
         "$(date +%H:%M:%S)" "$CT" "$GE" "$GJ" "$GM" >> "$TLOG"
  if awk -v a="$CT" -v b="$GE" -v c="$GJ" -v d="$GM" \
       'BEGIN { exit !(a>=86 || b>=92 || c>=105 || d>=100) }'; then
    echo "THERMAL KILL $(date +%H:%M:%S) cpu=$CT edge=$GE junction=$GJ mem=$GM" >> "$TLOG"
    kill -TERM -"$PGID" 2>/dev/null
    sleep 5
    kill -KILL -"$PGID" 2>/dev/null
    KILLED=1
    break
  fi
  if awk -v a="$CT" -v b="$GE" -v c="$GJ" -v d="$GM" \
       'BEGIN { exit !(a>=78 || b>=85 || c>=95 || d>=92) }'; then
    echo "WARN $(date +%H:%M:%S) cpu=$CT edge=$GE junction=$GJ mem=$GM" >> "$TLOG"
  fi
  sleep 30
done
wait "$HPID" 2>/dev/null
echo "guard: done killed=$KILLED at $(date +%H:%M:%S)"
echo "=== thermal (last 12) ==="; tail -12 "$TLOG"
echo "=== run log (last 30) ==="; tail -30 "$RUNLOG"
