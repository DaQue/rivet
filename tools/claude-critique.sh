#!/usr/bin/env bash
# Run ONE read-only critique of the rivet port through Claude Code on David's Claude Pro
# subscription (plain `claude`), NOT claude-ds (DeepSeek pay-per-token API) and NOT the local
# qwen shim. The env unsets below are the guarantee: if any of them were set, `claude` would
# bill an API key instead of using the subscription login in ~/.claude/.credentials.json.
#
# Usage: tools/claude-critique.sh <prompt-file> [tag]
set -uo pipefail

REPO=/home/davidq/Projects/rivet
PROMPT_FILE=${1:?usage: claude-critique.sh <prompt-file> [tag]}
TAG=${2:-critique}
LOGDIR=/home/davidq/.hermes/cache/scratch/qwen-impl
TS=$(date +%Y%m%d-%H%M%S)
LOG="$LOGDIR/CRITIQUE-$TAG.run-$TS.log"
FINAL="$LOGDIR/CRITIQUE-$TAG.$TS.md"
mkdir -p "$LOGDIR"

cd "$REPO" || exit 1

# --- the whole point: subscription auth only -----------------------------------------------
unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL ANTHROPIC_MODEL
unset CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_VERTEX
export PATH="/home/davidq/.local/bin:$PATH"

{
  echo "=== critique '$TAG' start $(date -Is) ==="
  echo "=== repo=$REPO HEAD=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l) file(s) ==="
  echo "=== auth: subscription login present: $(test -f "$HOME/.claude/.credentials.json" && echo yes || echo NO) | ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:-<unset>} | BASE_URL=${ANTHROPIC_BASE_URL:-<unset>} ==="
  echo "=== claude: $(command -v claude) ($(claude --version 2>&1 | head -1)) ==="
} | tee -a "$LOG"

# Read-only: reading/searching/git+cargo inspection allowed; editing, committing and pushing are
# explicitly denied so the critique cannot change the tree or touch a remote.
claude -p "$(cat "$PROMPT_FILE")" \
  --max-turns 80 \
  --output-format stream-json --verbose \
  --allowedTools 'Read,Grep,Glob,Bash(git diff:*),Bash(git log:*),Bash(git show:*),Bash(git status:*),Bash(git hash-object:*),Bash(git cat-file:*),Bash(cargo check:*),Bash(cargo test:*),Bash(cargo clippy:*),Bash(printf:*),Bash(sha1sum:*)' \
  --disallowedTools 'Edit,Write,NotebookEdit,Bash(git commit:*),Bash(git push:*),Bash(git remote:*),Bash(git add:*),Bash(rm:*)' \
  >>"$LOG" 2>&1
CRITIQUE_EXIT=$?

# Pull the final assistant text out of the stream-json log into a readable markdown file.
python3 - "$LOG" > "$FINAL" <<'PY'
import json, sys
result = None
for line in open(sys.argv[1], errors="replace"):
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        obj = json.loads(line)
    except Exception:
        continue
    if obj.get("type") == "result":
        result = obj
if result is None:
    print("(no result event found in the stream log)")
else:
    if result.get("is_error"):
        print(f"**claude reported an error**: {result.get('subtype')} / {result.get('result')}\n")
    print(result.get("result") or "(empty result)")
    print(f"\n---\nturns={result.get('num_turns')} duration_ms={result.get('duration_ms')} "
          f"cost_usd={result.get('total_cost_usd')} (subscription: no per-token charge)")
PY

echo "CRITIQUE_EXIT=$CRITIQUE_EXIT"
echo "CRITIQUE_LOG=$LOG"
echo "CRITIQUE_FINAL=$FINAL"
echo "=== end $(date -Is) ==="
