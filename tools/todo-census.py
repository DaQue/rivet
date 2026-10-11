#!/usr/bin/env python3
"""Count real todo!()/unimplemented!() stub bodies in Rivet's Rust sources.

Strips // line comments (quote-aware) and /* */ blocks so prose in doc comments
does not inflate the count; then counts the macro invocations that remain.
Usage: python3 tools/todo-census.py [root]
"""
import os
import sys

root = sys.argv[1] if len(sys.argv) > 1 else "crates"
MACROS = ("todo!()", "unimplemented!()", "unreachable!()")


def strip_line(line, in_block):
    out = []
    i = 0
    quote = None
    while i < len(line):
        c = line[i]
        if in_block:
            if line.startswith("*/", i):
                in_block = False
                i += 2
                continue
            i += 1
            continue
        if quote:
            out.append(c)
            if c == "\\":
                if i + 1 < len(line):
                    out.append(line[i + 1])
                i += 2
                continue
            if c == quote:
                quote = None
            i += 1
            continue
        if c in "\"'":
            quote = c
            out.append(c)
            i += 1
            continue
        if line.startswith("//", i):
            break
        if line.startswith("/*", i):
            in_block = True
            i += 2
            continue
        out.append(c)
        i += 1
    return "".join(out), in_block


per_file = {}
per_macro = {m: 0 for m in MACROS}
for dirpath, _dirs, names in os.walk(root):
    for name in sorted(names):
        if not name.endswith(".rs"):
            continue
        path = os.path.join(dirpath, name)
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            text = fh.read()
        in_block = False
        hits = 0
        for raw in text.splitlines():
            code, in_block = strip_line(raw, in_block)
            for m in MACROS:
                if m in code:
                    n = code.count(m)
                    hits += n
                    per_macro[m] += n
        if hits:
            per_file[path] = hits

total = sum(per_file.values())
print("ROOT=%s" % root)
print("STUB_BODIES_TOTAL=%d" % total)
for m in MACROS:
    print("  %-18s %d" % (m, per_macro[m]))
print("FILES=%d" % len(per_file))
for path, n in sorted(per_file.items(), key=lambda kv: (-kv[1], kv[0])):
    print("  %4d  %s" % (n, path))
