#!/usr/bin/env bash
# Burn-down: the only progress number that matters. See COMPLETION.md.
# Counts open bullets in GAPS.md §1 / §2 / defects, appends a dated line to .claude/burndown.tsv
# and prints the delta against the previous line.
set -u
REPO=$(cd "$(dirname "$0")/../.." && pwd)
GAPS=${GAPS:-$REPO/GAPS.md}
TSV=${TSV:-$REPO/.claude/burndown.tsv}
mkdir -p "$(dirname "$TSV")"

read -r s1 s2 defects < <(awk '
  /^## 1\. Not implemented/   { s = 1; next }
  /^## 2\. Implemented/       { s = 2; next }
  /^## 3\. Deliberate/        { s = 0; next }
  /^## Known defects/         { s = 3; next }
  /^## Flaky/                 { s = 0; next }
  /^\* \*\*/                  { c[s]++ }
  END { printf "%d %d %d\n", c[1] + 0, c[2] + 0, c[3] + 0 }
' "$GAPS")

open=$((s1 + s2 + defects))
prev=""
[ -f "$TSV" ] && prev=$(tail -1 "$TSV")
printf '%s\t%d\t%d\t%d\t%d\n' "$(date +%F)" "$s1" "$s2" "$defects" "$open" >>"$TSV"

if [ -n "$prev" ]; then
  read -r _ p1 p2 pd po <<<"$prev"
  printf 'burndown %s: §1 %d (%+d)  §2 %d (%+d)  defects %d (%+d)  open %d (%+d)\n' \
    "$(date +%F)" "$s1" $((s1 - p1)) "$s2" $((s2 - p2)) "$defects" $((defects - pd)) "$open" $((open - po))
else
  printf 'burndown %s: §1 %d  §2 %d  defects %d  open %d (baseline)\n' "$(date +%F)" "$s1" "$s2" "$defects" "$open"
fi
