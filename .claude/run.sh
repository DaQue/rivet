#!/usr/bin/env bash
# Autonomous rivet completion run.
# Uses the Claude Code subscription login (no ANTHROPIC_API_KEY in env).
set -u
cd /data/rust/rivet || exit 1
LOG=/data/rust/rivet/.claude/run.log
{
  echo "=== run start: $(date -Is) model=sonnet resume=1 ==="
  claude -p "Continue the mission from exactly where you left off. Re-read .claude/HANDOFF.md and PROGRESS.md, check 'git status' and 'cargo check --workspace', finish whatever was mid-flight, then keep working the checklist through Phase 3. Do not stop to ask questions." \
    --continue \
    --dangerously-skip-permissions \
    --model sonnet \
    --max-turns 800 \
    --verbose \
    --output-format text
  echo "=== run end: $(date -Is) exit=$? ==="
} 2>&1 | tee -a "$LOG"
