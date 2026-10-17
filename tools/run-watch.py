#!/usr/bin/env python3
"""Watch a qwen-batch run: per-run function completions + GPU temperatures.

Prints "RUN #1", "RUN #2", ... for functions that had a stub body in the
baseline snapshot and no longer do. Numbering is per run and always starts at 1,
independent of any lifetime/cumulative count.

Writes:
  <out>/RUN-<stamp>.ledger    "#N  HH:MM:SS  <file>::<fn>#<occ>  [task]"
  <out>/RUN-<stamp>.progress  one line per poll (temps, stub count, run count)
  <out>/RUN-<stamp>.summary   final tally
"""
import argparse
import glob
import json
import os
import re
import subprocess
import time
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))


def temp_of(label):
    try:
        txt = subprocess.run(["sensors"], capture_output=True, text=True, timeout=20).stdout
    except Exception:
        return None
    vals = []
    for line in txt.splitlines():
        if line.strip().startswith(label + ":"):
            m = re.search(r"\+([0-9.]+)", line)
            if m:
                vals.append(float(m.group(1)))
    return max(vals) if vals else None


def census(root):
    p = subprocess.run(["python3", os.path.join(HERE, "stub-fns.py"), "--json", "-", root],
                       capture_output=True, text=True, timeout=300)
    if p.returncode != 0:
        return None
    return set(json.loads(p.stdout)["stubs"])


def newest(pattern):
    hits = sorted(glob.glob(pattern), key=os.path.getmtime)
    return hits[-1] if hits else None


def current_task(out):
    f = newest(os.path.join(out, "*.thermal-*.log"))
    if not f:
        return "-"
    m = re.search(r"\.([A-Za-z0-9_]+)\.thermal-", os.path.basename(f))
    return m.group(1) if m else "-"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--baseline", required=True)
    ap.add_argument("--hours", type=float, default=4.0)
    ap.add_argument("--stamp", required=True)
    ap.add_argument("--out", default=os.path.expanduser("~/.hermes/cache/scratch/qwen-impl"))
    ap.add_argument("--root", default=None, help="repo root (default: parent of tools/)")
    ap.add_argument("--interval", type=int, default=60)
    a = ap.parse_args()

    root = a.root or os.path.dirname(HERE)
    os.makedirs(a.out, exist_ok=True)
    ledger = os.path.join(a.out, "RUN-%s.ledger" % a.stamp)
    progress = os.path.join(a.out, "RUN-%s.progress" % a.stamp)
    summary = os.path.join(a.out, "RUN-%s.summary" % a.stamp)

    base = set(json.load(open(a.baseline))["stubs"])
    seen, n = set(), 0
    deadline = time.time() + a.hours * 3600
    status_path = newest(os.path.join(a.out, "BATCH-*.status"))
    status_id = (status_path, os.path.getmtime(status_path)) if status_path else None

    def log(path, line):
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(line + "\n")

    log(progress, "start %s baseline_stubs=%d" % (datetime.now().strftime("%H:%M:%S"), len(base)))
    print("watch start: baseline %d stubs, %.1f h" % (len(base), a.hours), flush=True)

    while time.time() < deadline:
        cur = census(root)
        hr = datetime.now().strftime("%H:%M:%S")
        task = current_task(a.out)
        if cur is None:
            log(progress, "%s task=%s CENSUS_FAILED" % (hr, task))
            time.sleep(a.interval)
            continue

        fresh = sorted((base - cur) - seen)
        for key in fresh:
            n += 1
            seen.add(key)
            line = "RUN #%-3d %s  %s  [%s]" % (n, hr, key, task)
            log(ledger, line)
            print(line, flush=True)

        e, j, m = temp_of("edge"), temp_of("junction"), temp_of("mem")
        line = ("%s task=%s stubs=%d run_done=%d temp_edge=%s temp_junction=%s temp_mem=%s"
                % (hr, task, len(cur), n, e, j, m))
        log(progress, line)
        print(line, flush=True)

        st = newest(os.path.join(a.out, "BATCH-*.status"))
        if st and st != status_path:
            status_path = st
            status_id = (st, os.path.getmtime(st))
        if status_path and os.path.exists(status_path):
            if status_id is None or status_id[0] != status_path:
                status_id = (status_path, 0.0)
            stamp_now = os.path.getmtime(status_path)
            tail = open(status_path, errors="replace").read().splitlines()[-4:]
            finished = any("BATCH end" in t or "BATCH done" in t for t in tail)
            if finished and stamp_now > status_id[1]:
                log(progress, "%s batch finished" % hr)
                break
        time.sleep(a.interval)

    lines = ["RUN functions completed: %d" % n,
             "baseline stubs: %d" % len(base),
             "ledger: %s" % ledger,
             "progress: %s" % progress]
    with open(summary, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines) + "\n")
    print("\n".join(lines), flush=True)


if __name__ == "__main__":
    main()
