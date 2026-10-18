#!/usr/bin/env python3
"""bodyfill.py — deterministic per-function filler for `todo!()` stubs.

The failure mode this replaces: an agent handed a 1,600-line file re-reads it for
45 minutes and never edits.  Here the model is asked for ONE function body at a
time as plain text, with no tools at all.  The script splices the body in, runs
`cargo check`, and reverts anything red.  Edit decisions are deterministic; only
the code text comes from the model.

Speed SLO (self-enforced, from the operator):
  * no function completed in 15 min  -> abort
  * fewer than 3 functions in 30 min -> abort
A run that cannot clear that is not worth the electricity.

Usage:
  bodyfill.py [--file PATH] [--max-functions N] [--max-hours H] [--dry-run]
"""
import argparse
import json
import os
import re
import subprocess
import sys
import textwrap
import time
import urllib.request
from datetime import datetime

REPO = "/home/davidq/Projects/rivet"
OUT = os.path.expanduser("~/.hermes/cache/scratch/qwen-impl")
MODEL = os.environ.get("BODYFILL_MODEL", "qwen38-gsq-fast:latest")
OLLAMA = os.environ.get("OLLAMA_URL", "http://127.0.0.1:11434")

FN_RE = re.compile(
    r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:unsafe\s+)?"
    r"(?:extern\s+\"[^\"]*\"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)
IMPL_RE = re.compile(r"^impl(?:<[^>]*>)?\s+([A-Za-z_][A-Za-z0-9_]*)")


# ---------------------------------------------------------------- file scanning
def line_offsets(lines):
    offs, n = [], 0
    for ln in lines:
        offs.append(n)
        n += len(ln) + 1
    return offs


def scan_body(text, start):
    """start points at '{'. Return index of the matching '}'."""
    depth, i, n = 0, start, len(text)
    while i < n:
        c = text[i]
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            j = text.find("\n", i)
            i = n if j < 0 else j + 1
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "*":
            j = text.find("*/", i + 2)
            i = n if j < 0 else j + 2
            continue
        if c == '"':
            i += 1
            while i < n:
                if text[i] == "\\":
                    i += 2
                    continue
                if text[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if c == "'":
            if i + 1 < n and text[i + 1] == "\\":
                j = text.find("'", i + 2)
                i = n if j < 0 else j + 1
                continue
            if i + 2 < n and text[i + 2] == "'":
                i += 3
                continue
            i += 1
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def find_stubs(text):
    lines = text.split("\n")
    offs = line_offsets(lines)
    found = []
    for i, ln in enumerate(lines):
        if ln.strip() != "todo!()":
            continue
        j, m = i, None
        while j >= 0 and j > i - 250:
            m = FN_RE.match(lines[j])
            if m:
                break
            j -= 1
        if not m:
            continue
        d = j
        while d - 1 >= 0:
            s = lines[d - 1].strip()
            if s.startswith("///") or s.startswith("#[") or s.startswith("//!"):
                d -= 1
            else:
                break
        k = text.find("{", offs[j])
        if k < 0:
            continue
        close = scan_body(text, k)
        if close < 0:
            continue
        close_line = text.count("\n", 0, close)
        found.append(
            {
                "name": m.group(2),
                "indent": m.group(1),
                "fn_line": j,
                "doc_line": d,
                "open": k,
                "close": close,
                "close_line": close_line,
            }
        )
    return found


def type_context(text, lines, fn_line):
    """Struct/type definition for the enclosing impl, so field names are known."""
    name = None
    for i in range(fn_line, -1, -1):
        m = IMPL_RE.match(lines[i])
        if m:
            name = m.group(1)
            break
    if not name:
        return ""
    pat = re.compile(r"^pub(?:\([^)]*\))?\s*(?:struct|enum)\s+" + re.escape(name) + r"\b")
    for i, ln in enumerate(lines):
        if pat.match(ln):
            k = text.find("{", line_offsets(lines)[i])
            if k < 0:
                return ""
            close = scan_body(text, k)
            if close < 0:
                return ""
            end = text.count("\n", 0, close)
            return "\n".join(lines[i : min(end + 1, i + 45)])
    return ""


def imports(text):
    keep = [
        ln
        for ln in text.split("\n")
        if ln.startswith("use ") or ln.startswith("pub use ") or ln.strip().startswith("use crate")
    ]
    return "\n".join(keep[:28])


def build_prompt(text, lines, stub):
    i, d = stub["fn_line"], stub["doc_line"]
    k = stub["open"]
    sig = text[line_offsets(lines)[i] : k].rstrip().rstrip("{").rstrip()
    doc = "\n".join(l.strip().lstrip("/").strip() for l in lines[d:i])
    ctx = "\n".join(lines[max(0, d - 14) : d])
    parts = [
        "You are filling in ONE Rust function body in a git-compatible library.\n"
        "\n"
        "Rules:\n"
        "- Output ONLY the body statements: no signature, no outer braces, no markdown\n"
        "  fences, no explanation, no comments about what you did.\n"
        "- Use the exact field names, types and helper functions shown in the context.\n"
        "- The body must compile as Rust 2021. Do not add dependencies.\n"
        "- Never write todo!() or unimplemented!().\n",
        "TYPE DEFINITION:\n" + type_context(text, lines, i),
        "IMPORTS IN SCOPE:\n" + imports(text),
        "FUNCTION SIGNATURE (already written, do not repeat it):\n" + sig,
        "DOCUMENTED CONTRACT (this is the specification):\n" + doc,
        "NEARBY CONTEXT:\n" + ctx,
        "\nReturn the body for `" + stub["name"] + "` now.",
    ]
    return "\n\n".join(p for p in parts if p.strip())


# ---------------------------------------------------------------- model + tools
def ask(messages, timeout=420):
    payload = {
        "model": MODEL,
        "stream": False,
        "think": False,
        "messages": messages,
        "options": {
            "temperature": 0.15,
            "top_p": 0.9,
            "num_predict": 1400,
            "num_ctx": 16384,
        },
    }
    req = urllib.request.Request(
        OLLAMA + "/api/chat",
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        d = json.load(r)
    return (
        d.get("message", {}).get("content", ""),
        d.get("prompt_eval_count", 0),
        d.get("eval_count", 0),
    )

def clean_body(resp, indent):
    s = resp.strip()
    m = re.search(r"```(?:rust)?\s*\n(.*?)```", s, re.S)
    if m:
        s = m.group(1)
    if re.search(r"\bfn\s+[A-Za-z_]\w*\s*\(", s):
        k = s.find("{")
        if k >= 0:
            s = s[k + 1 :]
    s = s.strip()
    # drop a trailing brace only if it is the function's closer (unbalanced)
    if s.endswith("}"):
        depth = 0
        for ch in s:
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
        if depth < 0:
            s = s.rstrip()[:-1].rstrip()
    s = textwrap.dedent(s).strip("\n")
    if not s or "todo!()" in s:
        return ""
    # Re-indent from brace depth so output is canonical regardless of how the
    # model spaced it.  Braces only: multi-line calls stay on one indent level.
    out, depth = [], 0
    for raw in s.split("\n"):
        t = raw.strip()
        if not t:
            out.append("")
            continue
        d = max(0, depth - 1) if t[0] in "})]" else depth
        out.append(" " * (indent + 4 * d) + t)
        depth = max(0, depth + t.count("{") - t.count("}"))
    return "\n".join(out)


def errs_text(out, src):
    """Compiler errors plus the offending source lines, so a repair is possible."""
    parts = []
    for l in out.split("\n"):
        if "error" not in l or ": " not in l:
            continue
        parts.append(l)
        m = re.match(r".*?:(\d+):\d+: error", l)
        if m:
            n = int(m.group(1))
            sl = src.split("\n")
            for k in range(max(0, n - 2), min(len(sl), n + 1)):
                parts.append(f"  {k + 1}| {sl[k]}")
        if len(parts) > 40:
            break
    return "\n".join(parts)[:2500]


def cargo(args, timeout=600):
    p = subprocess.run(
        ["cargo"] + args,
        cwd=REPO,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return p.returncode, (p.stdout or "") + (p.stderr or "")


def temps():
    out = {}
    for lab in ("edge", "junction", "mem"):
        for p in os.listdir("/sys/class/drm") if os.path.isdir("/sys/class/drm") else []:
            base = f"/sys/class/drm/{p}/device/hwmon"
            if not os.path.isdir(base):
                continue
            for hw in os.listdir(base):
                d = os.path.join(base, hw)
                for f in os.listdir(d) if os.path.isdir(d) else []:
                    if f.endswith("_label"):
                        try:
                            if open(os.path.join(d, f)).read().strip() != lab:
                                continue
                            v = int(open(os.path.join(d, f[:-6] + "_input")).read().strip())
                            out[lab] = v / 1000.0
                        except Exception:
                            pass
    return out


# ---------------------------------------------------------------------- driver
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--file", default="crates/rivet-core/src/index/mod.rs")
    ap.add_argument("--max-functions", type=int, default=25)
    ap.add_argument("--max-hours", type=float, default=4.0)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    path = os.path.join(REPO, args.file)
    os.makedirs(OUT, exist_ok=True)
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    logp = os.path.join(OUT, f"BODYFILL-{stamp}.log")
    prog = os.path.join(OUT, f"BODYFILL-{stamp}.progress")

    def log(msg):
        line = f"{datetime.now():%H:%M:%S} {msg}"
        print(line, flush=True)
        with open(logp, "a") as f:
            f.write(line + "\n")

    rc, out = cargo(["check", "-p", "rivet-core", "--all-targets", "--message-format=short"])
    if rc != 0:
        log("ABORT: tree is not green before we start")
        print(out[-3000:])
        return 2
    log(f"start file={args.file} model={MODEL} max={args.max_hours}h")

    t0 = time.time()
    done = 0
    consecutive_fail = 0
    skipped = set()

    while done < args.max_functions:
        elapsed = time.time() - t0
        if elapsed > args.max_hours * 3600:
            log(f"STOP deadline {args.max_hours}h done={done}")
            break
        if done == 0 and elapsed > 15 * 60:
            log(f"STOP SLO: 0 functions in {elapsed/60:.0f} min (budget 15)")
            break
        if done < 3 and elapsed > 30 * 60:
            log(f"STOP SLO: {done} functions in {elapsed/60:.0f} min (need 3)")
            break

        t = temps()
        if (t.get("edge", 0) > 92) or (t.get("junction", 0) > 105) or (t.get("mem", 0) > 100):
            log(f"STOP thermal edge={t.get('edge')} junction={t.get('junction')} mem={t.get('mem')}")
            break

        text = open(path, encoding="utf-8").read()
        lines = text.split("\n")
        stubs = find_stubs(text)
        if not stubs:
            log(f"DONE no todo!() left in {args.file}")
            break
        todo_stubs = [s for s in stubs if s["name"] not in skipped]
        if not todo_stubs:
            log(f"DONE every remaining stub skipped: {sorted(skipped)}")
            break
        stub = todo_stubs[0]
        log(f"fn={stub['name']} line={stub['fn_line']+1} remaining={len(stubs)}")

        prompt = build_prompt(text, lines, stub)
        with open(os.path.join(OUT, f"prompt-{stub['name']}.txt"), "w") as f:
            f.write(prompt)
        msgs = [
            {
                "role": "system",
                "content": "You write Rust function bodies only. No prose, no fences.",
            },
            {"role": "user", "content": prompt},
        ]

        fstart = time.time()
        if args.dry_run:
            body, pt, et = ask(msgs)
            print("=== PROMPT ===\n" + prompt)
            print("=== RAW ===\n" + body)
            print("=== CLEAN ===\n" + clean_body(body, len(stub["indent"]) + 4))
            print(f"prefill={pt} gen={et} t={time.time()-fstart:.0f}s")
            return 0

        ok = False
        for attempt in (1, 2):
            try:
                body, pt, et = ask(msgs)
            except Exception as e:
                log(f"  attempt{attempt} model error: {e}")
                time.sleep(5)
                continue
            code = clean_body(body, len(stub["indent"]) + 4)
            if not code:
                log(f"  attempt{attempt} unusable output ({et} tok)")
                msgs = msgs[:2] + [
                    {
                        "role": "user",
                        "content": "That was empty or still a todo!(). Return the body statements only.",
                    }
                ]
                continue

            new = text[: stub["open"] + 1] + "\n" + code + "\n" + stub["indent"] + text[stub["close"] :]
            open(path, "w", encoding="utf-8").write(new)
            rc, out = cargo(["check", "-p", "rivet-core", "--all-targets", "--message-format=short"])
            if rc == 0:
                ok = True
                log(f"  attempt{attempt} CHECK OK prefill={pt} gen={et} t={time.time()-fstart:.0f}s")
                break
            errs = errs_text(out, new)
            log(f"  attempt{attempt} CHECK FAIL {len(errs)} chars of errors")
            with open(os.path.join(OUT, f"fail-{stub['name']}-{attempt}.txt"), "w") as f:
                f.write(errs + "\n\n=== BODY SENT ===\n" + body + "\n\n=== SPLICED ===\n" + new)
            open(path, "w", encoding="utf-8").write(text)
            msgs = msgs + [
                {"role": "assistant", "content": body},
                {
                    "role": "user",
                    "content": "That body failed to compile:\n" + errs + "\n\nReturn the corrected body only.",
                },
            ]

        if not ok:
            open(path, "w", encoding="utf-8").write(text)
            skipped.add(stub["name"])
            consecutive_fail += 1
            log(f"  fn={stub['name']} FAILED skipped (consecutive={consecutive_fail}, skipped={len(skipped)})")
            with open(prog, "a") as f:
                f.write(f"{datetime.now():%H:%M:%S} fail fn={stub['name']} done={done}\n")
            if consecutive_fail >= 4:
                log("STOP 4 consecutive failures - systemic, not a slow model")
                break
            continue

        consecutive_fail = 0
        done += 1
        subprocess.run(["git", "add", "-A", "--", "crates"], cwd=REPO, capture_output=True)
        subprocess.run(
            ["git", "commit", "-q", "-m", f"bodyfill: {stub['name']} (Index surface)"],
            cwd=REPO,
            capture_output=True,
        )
        left = len(find_stubs(open(path, encoding="utf-8").read()))
        el = time.time() - t0
        log(f"  RUN #{done} fn={stub['name']} ok stubs_left={left} elapsed={el/60:.1f}m")
        with open(prog, "a") as f:
            f.write(
                f"{datetime.now():%H:%M:%S} done={done} fn={stub['name']} left={left} {el/60:.1f}m\n"
            )

    log(f"end done={done} elapsed={(time.time()-t0)/60:.1f}m")
    return 0


if __name__ == "__main__":
    sys.exit(main())
