#!/usr/bin/env python3
"""Rivet census: Rust LOC + function-level implementation status.

For every .rs file under the given roots it reports
  * lines: total / blank / comment-only / code
  * functions: implemented (no todo!()), stub-only (todo!() is the whole body),
    mixed (real statements AND a todo!()), and bodyless declarations.

Comment stripping is quote aware; brace matching runs on a copy with string
contents masked, so `todo!()` inside a doc comment or a string never counts.
Usage: python3 tools/loc-census.py
"""
import os
import re
import sys

ROOTS = sys.argv[1:] or ["crates"]
FN_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
TODO = "todo!()"
UNIMPL = "unimplemented!()"
CHAR_RE = re.compile(r"'(?:\\.|[^\\'\n])'")


def scan(text):
    """Return (no_comments, masked): comments blanked; masked also blanks strings."""
    nc = list(text)
    masked = list(text)
    i, n = 0, len(text)
    state = None
    while i < n:
        c = text[i]
        if state == "line":
            if c == "\n":
                state = None
            else:
                nc[i] = masked[i] = " "
            i += 1
            continue
        if state == "block":
            if text.startswith("*/", i):
                nc[i] = nc[i + 1] = masked[i] = masked[i + 1] = " "
                i += 2
                state = None
                continue
            if c != "\n":
                nc[i] = masked[i] = " "
            i += 1
            continue
        if state in ("str", "chr"):
            if c == "\\":
                masked[i] = " "
                if i + 1 < n and text[i + 1] != "\n":
                    masked[i + 1] = " "
                i += 2
                continue
            if (state == "str" and c == '"') or (state == "chr" and c == "'"):
                masked[i] = " "
                state = None
                i += 1
                continue
            if c != "\n":
                masked[i] = " "
            i += 1
            continue
        if text.startswith("//", i):
            nc[i] = nc[i + 1] = masked[i] = masked[i + 1] = " "
            i += 2
            state = "line"
            continue
        if text.startswith("/*", i):
            nc[i] = nc[i + 1] = masked[i] = masked[i + 1] = " "
            i += 2
            state = "block"
            continue
        if c == '"':
            state = "str"
            i += 1
            continue
        if c == "'":
            # a lifetime ('a, 'static) or label ('outer:) is NOT a char literal;
            # only treat it as one when it closes on the same shape as 'x'/'\n'
            if CHAR_RE.match(text, i):
                state = "chr"
                i += 1
            else:
                i += 1
            continue
        i += 1
    return "".join(nc), "".join(masked)


def match_brace(s, open_idx):
    depth = 0
    for j in range(open_idx, len(s)):
        if s[j] == "{":
            depth += 1
        elif s[j] == "}":
            depth -= 1
            if depth == 0:
                return j
    return -1


def functions(nc, masked):
    out = []
    for m in FN_RE.finditer(masked):
        name = m.group(1)
        # skip call sites like `foo(x)` shadowed by the word fn; FN_RE needs 'fn'
        k = m.end() - 1
        depth = 0
        body_open = None
        decl_end = None
        while k < len(masked):
            ch = masked[k]
            if ch in "([<":
                depth += 1
            elif ch in ")]>":
                depth = max(0, depth - 1)
            elif ch == "{" and depth == 0:
                body_open = k
                break
            elif ch == ";" and depth == 0:
                decl_end = k
                break
            k += 1
        if body_open is None:
            out.append((name, "declaration", "", masked.count("\n", 0, m.start()) + 1))
            continue
        close = match_brace(masked, body_open)
        if close < 0:
            out.append((name, "unparsed", "", masked.count("\n", 0, m.start()) + 1))
            continue
        body = nc[body_open + 1 : close]
        out.append((name, "body", body, masked.count("\n", 0, m.start()) + 1))
    return out


def classify(body):
    has_todo = TODO in body or UNIMPL in body
    rest = body.replace(TODO, " ").replace(UNIMPL, " ")
    meaningful = re.sub(r"[\s;{}()]", "", rest)
    # strip attributes / allow() noise already gone; any identifier left is real code
    if not has_todo:
        return "implemented"
    return "stub-only" if not meaningful else "mixed"


areas = {}
fn_counts = {k: 0 for k in ("implemented", "implemented-test", "stub-only", "mixed", "declaration", "unparsed")}
fn_by_area = {}
fn_by_file = {}
for root in ROOTS:
    for dirpath, _dirs, names in os.walk(root):
        for name in sorted(names):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            with open(path, "r", encoding="utf-8", errors="replace") as fh:
                text = fh.read()
            nc, masked = scan(text)
            lines = nc.splitlines()
            orig = text.splitlines()
            total = len(lines)
            blank = 0
            comment_only = 0
            for idx, ln in enumerate(lines):
                raw = orig[idx] if idx < len(orig) else ""
                if not raw.strip():
                    blank += 1
                elif not ln.strip():
                    comment_only += 1
            code = total - blank - comment_only
            # area = crate + src|tests
            parts = path.split(os.sep)
            crate = parts[1] if len(parts) > 1 else "?"
            kind = "tests" if "tests" in parts[2:-1] or "/tests/" in path else "src"
            area = "%s/%s" % (crate, kind)
            a = areas.setdefault(area, {"files": 0, "total": 0, "blank": 0, "comment": 0, "code": 0})
            a["files"] += 1
            a["total"] += total
            a["blank"] += blank
            a["comment"] += comment_only
            a["code"] += code
            fa = fn_by_area.setdefault(area, {k: 0 for k in fn_counts})
            fb = fn_by_file.setdefault(path, {k: 0 for k in fn_counts})
            tests_line = None
            for idx, ln in enumerate(nc.splitlines()):
                if re.match(r"\s*(pub\s+)?mod\s+tests\b", ln):
                    tests_line = idx + 1
                    break
            for fname, status, body, lineno in functions(nc, masked):
                if status == "body":
                    c = classify(body)
                    if c == "implemented" and tests_line and lineno > tests_line:
                        c = "implemented-test"
                else:
                    c = status
                fa[c] = fa.get(c, 0) + 1
                fb[c] = fb.get(c, 0) + 1
                fn_counts[c] = fn_counts.get(c, 0) + 1

print("== LINES (comment-stripped counts; strings intact) ==")
print("%-22s %5s %7s %7s %8s %7s" % ("area", "files", "total", "blank", "comment", "code"))
for area in sorted(areas):
    a = areas[area]
    print("%-22s %5d %7d %7d %8d %7d" % (area, a["files"], a["total"], a["blank"], a["comment"], a["code"]))
tot = {k: sum(a[k] for a in areas.values()) for k in ("files", "total", "blank", "comment", "code")}
print("%-22s %5d %7d %7d %8d %7d" % ("ALL", tot["files"], tot["total"], tot["blank"], tot["comment"], tot["code"]))

print()
print("== FUNCTIONS ==")
print("%-22s %13s %16s %10s %7s %12s" % ("area", "implemented", "impl-in-tests", "stub-only", "mixed", "declaration"))
for area in sorted(fn_by_area):
    f = fn_by_area[area]
    print("%-22s %13d %16d %10d %7d %12d" % (area, f["implemented"], f["implemented-test"], f["stub-only"], f["mixed"], f["declaration"]))
print("%-22s %13d %16d %10d %7d %12d" % ("ALL", fn_counts["implemented"], fn_counts["implemented-test"], fn_counts["stub-only"], fn_counts["mixed"], fn_counts["declaration"]))
if fn_counts["unparsed"]:
    print("unparsed: %d" % fn_counts["unparsed"])

print()
print("== TODO SITES vs STUB FUNCTIONS ==")
sites_total = sites_in_fn = 0
multi = []
for root in ROOTS:
    for dirpath, _dirs, names in os.walk(root):
        for name in sorted(names):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            with open(path, "r", encoding="utf-8", errors="replace") as fh:
                text = fh.read()
            nc, masked = scan(text)
            sites = nc.count(TODO) + nc.count(UNIMPL)
            sites_total += sites
            inside = 0
            for fname, status, body, _lineno in functions(nc, masked):
                if status != "body":
                    continue
                k = body.count(TODO) + body.count(UNIMPL)
                inside += k
                if k > 1 or (k == 1 and classify(body) == "mixed"):
                    multi.append((k, "%s:%s" % (path, fname), classify(body)))
            sites_in_fn += inside
            if sites != inside:
                print("  sites-not-in-fn-body: %d of %d in %s" % (sites - inside, sites, path))
print("  todo!/unimplemented! sites total : %d" % sites_total)
print("  sites inside a parsed fn body    : %d" % sites_in_fn)
print("  functions with 2+ sites / mixed  : %d" % len(multi))
for k, where, cls in sorted(multi, key=lambda t: -t[0])[:12]:
    print("     %d x todo!()  %-10s %s" % (k, cls, where))

if "--per-file" in sys.argv:
    print()
    print("== PER FILE: functions (implemented / stub-only / declaration) ==")
    rows = []
    for path in sorted(fn_by_file):
        f = fn_by_file[path]
        rows.append((f["stub-only"], path, f))
    for _s, path, f in sorted(rows, key=lambda t: (t[0], t[1])):
        rel = path.replace(os.sep, "/")
        print("  done=%-3d stub=%-3d decl=%-2d  %s" % (f["implemented"], f["stub-only"], f["declaration"], rel))
