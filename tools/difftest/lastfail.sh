#!/usr/bin/env bash
# Print the diffs of the most recent failed scenario(s): lastfail.sh [N]
for d in $(ls -dt "${TMPDIR:-/tmp}"/difftest.* ~/.hermes/cache/scratch/difftest.* 2>/dev/null | head -"${1:-1}"); do echo "== $d"; cat "$d"/*.diff; done
