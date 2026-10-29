#!/usr/bin/env bash
# Build the blame benchmark fixture (3000 commits, ~44k objects) with real git and time
# `git blame` against `rivet blame` on it. Not part of the gate: timing is reported, never asserted.
# Usage: blame_bench.sh [dir]   Env: RIVET=/path/to/rivet, COMMITS=3000
set -eu
RIVET=${RIVET:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/release/rivet}
COMMITS=${COMMITS:-3000}
DIR=${1:-$(mktemp -d "${TMPDIR:-/tmp}/blame-bench.XXXXXX")}
if [ ! -d "$DIR/repo/.git" ]; then
    mkdir -p "$DIR/repo"
    git init -q -b main "$DIR/repo"
    python3 -I - "$COMMITS" <<'PY' | git -C "$DIR/repo" fast-import --quiet
import random, sys
n = int(sys.argv[1]); random.seed(7)
big = ["line %d initial" % i for i in range(1500)]
out = sys.stdout
def blob(mark, text):
    data = text.encode()
    out.write("blob\nmark :%d\ndata %d\n%s\n" % (mark, len(data), text))
mark = 0
for c in range(n):
    for _ in range(3):
        big[random.randrange(len(big))] = "line edited in commit %d" % c
    if c % 50 == 0:
        big.insert(random.randrange(len(big)), "inserted in commit %d" % c)
    mark += 1; blob(mark, "\n".join(big) + "\n"); bigmark = mark
    mark += 1; blob(mark, "payload %d\n" % c); small = mark
    mark += 1; blob(mark, "second %d\n" % c); small2 = mark
    msg = "commit %d" % c
    out.write("commit refs/heads/main\nmark :%d\nauthor A <a@x> %d +0000\ncommitter A <a@x> %d +0000\ndata %d\n%s\n" % (10**6 + c, 1600000000 + c * 60, 1600000000 + c * 60, len(msg), msg))
    if c: out.write("from :%d\n" % (10**6 + c - 1))
    out.write("M 100644 :%d big.txt\nM 100644 :%d d%d/e%d/f%d.txt\nM 100644 :%d d%d/g%d.txt\n" % (bigmark, small, c % 40, c % 7, c % 13, small2, c % 40, c % 11))
PY
fi
cd "$DIR/repo"; git checkout -q -f main 2>/dev/null || true
echo "objects: $(git count-objects -v | awk '/^count:/{c=$2} /^in-pack:/{p=$2} END{print c+p}') commits: $(git rev-list --count HEAD)"
for impl in git rivet; do
    start=$(date +%s.%N)
    if [ $impl = git ]; then git blame big.txt >"$DIR/$impl.out"; else timeout "${BENCH_TIMEOUT:-600}" "$RIVET" blame big.txt >"$DIR/$impl.out" || echo "rivet failed/timeout"; fi
    echo "$impl: $(awk -v a="$start" -v b="$(date +%s.%N)" 'BEGIN{printf "%.2f", b-a}') s"
done
cmp "$DIR/git.out" "$DIR/rivet.out" && echo "outputs identical"
echo "fixture: $DIR"
