#!/usr/bin/env python3
"""Function-level stub census for Rivet.

Reports which FUNCTIONS still carry a todo!()/unimplemented!()/unreachable!() body,
identified as "<relpath>::<fn_name>#<occurrence>" so a counter can tell which
functions finished during a single run (set difference against a baseline).

Comments and string/char/raw-string literals are blanked first, so prose such as
"replace every todo!()" in a doc comment never counts.

Usage: python3 tools/stub-fns.py [--json PATH|-] [root]
"""
import argparse
import json
import os
import re
import sys
import time

STUB_RE = re.compile(r"\b(todo|unimplemented|unreachable)!\s*\(")
FN_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")


def strip_literals(src):
    """Blank comments, strings, chars and raw strings; keep newlines in place."""
    out = list(src)
    i, n = 0, len(src)

    def blank(a, b):
        for k in range(a, min(b, n)):
            if src[k] != "\n":
                out[k] = " "

    while i < n:
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
            continue
        if src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth += 1
                    j += 2
                elif src.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank(i, j)
            i = j
            continue
        m = re.match(r'r(#*)"', src[i:])
        if m:
            close = '"' + m.group(1)
            j = src.find(close, i + len(m.group(0)))
            j = n if j < 0 else j + len(close)
            blank(i, j)
            i = j
            continue
        if src[i] == '"':
            j = i + 1
            while j < n:
                if src[j] == "\\":
                    j += 2
                    continue
                if src[j] == '"':
                    j += 1
                    break
                j += 1
            blank(i, j)
            i = j
            continue
        if src[i] == "'":
            ch = re.match(r"'([^'\\\n]|\\.)'", src[i:])
            if ch:
                blank(i, i + len(ch.group(0)))
                i += len(ch.group(0))
                continue
        i += 1
    return "".join(out)


def body_span(s, start):
    """Span of a fn body, or None for a declaration (no braces)."""
    p = s.find("(", start)
    if p < 0:
        return None
    depth, j = 0, p
    while j < len(s):
        if s[j] == "(":
            depth += 1
        elif s[j] == ")":
            depth -= 1
            if depth == 0:
                j += 1
                break
        j += 1
    while j < len(s):
        c = s[j]
        if c == "{":
            d, k = 0, j
            while k < len(s):
                if s[k] == "{":
                    d += 1
                elif s[k] == "}":
                    d -= 1
                    if d == 0:
                        return j, k + 1
                k += 1
            return j, len(s)
        if c == ";":
            return None
        j += 1
    return None


def census(root):
    stubs, per_file, all_fns = [], {}, 0
    for dirpath, dirnames, names in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in {"target", ".git", "backups"})
        for name in sorted(names):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            try:
                text = open(path, encoding="utf-8", errors="replace").read()
            except OSError:
                continue
            s = strip_literals(text)
            counts = {}
            for m in FN_RE.finditer(s):
                span = body_span(s, m.start())
                if span is None:
                    continue
                fname = m.group(1)
                counts[fname] = counts.get(fname, 0) + 1
                all_fns += 1
                if STUB_RE.search(s[span[0]:span[1]]):
                    stubs.append("%s::%s#%d" % (rel, fname, counts[fname]))
                    per_file[rel] = per_file.get(rel, 0) + 1
    return {
        "ts": int(time.time()),
        "total": len(stubs),
        "functions": all_fns,
        "stubs": sorted(stubs),
        "files": dict(sorted(per_file.items(), key=lambda kv: (-kv[1], kv[0]))),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", metavar="PATH", help="write JSON here ('-' = stdout)")
    ap.add_argument("root", nargs="?", default="crates")
    a = ap.parse_args()
    data = census(a.root)
    if a.json:
        blob = json.dumps(data, indent=1)
        if a.json == "-":
            print(blob)
        else:
            with open(a.json, "w", encoding="utf-8") as fh:
                fh.write(blob + "\n")
            print("STUB_FUNCTIONS=%d FUNCTIONS=%d FILES=%d -> %s"
                  % (data["total"], data["functions"], len(data["files"]), a.json))
        return
    print("STUB_FUNCTIONS=%d FUNCTIONS=%d FILES=%d" % (data["total"], data["functions"], len(data["files"])))
    for path, n in list(data["files"].items())[:25]:
        print("  %4d  %s" % (n, path))


if __name__ == "__main__":
    sys.exit(main())
