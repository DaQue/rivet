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


KEEP_TYPES = {
    "Index",
    "IndexEntry",
    "IndexExtension",
    "IndexVersion",
    "Stage",
    "IndexEntryFlags",
    "IndexEntryExtendedFlags",
    "EntryMode",
}
EXTRA_FILES = ["crates/rivet-core/src/object/mod.rs"]


def _strip_comments(block):
    return "\n".join(l for l in block.split("\n") if not l.strip().startswith("//"))


def _def_blocks(text):
    """Every `pub struct/enum NAME {...}` we keep, comments stripped."""
    lines = text.split("\n")
    offs = line_offsets(lines)
    out = []
    for i, ln in enumerate(lines):
        m = re.match(r"^pub\s+(?:struct|enum)\s+([A-Za-z_]\w*)", ln)
        if not m or m.group(1) not in KEEP_TYPES:
            continue
        k = text.find("{", offs[i])
        close = scan_body(text, k) if k >= 0 else -1
        if close < 0:
            continue
        end = text.count("\n", 0, close)
        out.append(_strip_comments("\n".join(lines[i : end + 1])))
    return out


def _sig(lines, i):
    s, j = lines[i].strip(), i
    while not re.search(r"[;{]", s) and j < i + 6:
        j += 1
        s += " " + lines[j].strip()
    s = re.split(r"[;{]", s)[0]
    return re.sub(r"\s+", " ", s).replace("pub ", "").replace("fn ", "", 1)


def file_index(src):
    """One-line index of public items, so the model can call helpers it can't see."""
    keep = []
    for ln in src.split("\n"):
        if re.match(r"^(pub (const )?(fn|struct|enum|const|type|trait)\s|    (pub )?(const )?fn )", ln):
            keep.append(ln.rstrip("{ ").rstrip())
    return "\n".join(keep[:45])


def imported_api(text):
    """Signatures from the modules this file imports - the helpers it must call."""
    mods = set(m.group(1) for m in re.finditer(r"use crate::([A-Za-z0-9_:]+)", text))
    mods |= set(
        m.group(1) + "::" + m.group(2)
        for m in re.finditer(r"\bcrate::([A-Za-z0-9_]+)::([A-Za-z0-9_]+)", text)
    )
    out = []
    for mod in sorted(mods)[:14]:
        base = mod.split("::")[0]
        for rel in (
            f"crates/rivet-core/src/{mod.replace('::', '/')}.rs",
            f"crates/rivet-core/src/{mod.replace('::', '/')}/mod.rs",
            f"crates/rivet-core/src/{base}.rs",
            f"crates/rivet-core/src/{base}/mod.rs",
        ):
            p = os.path.join(REPO, rel)
            if os.path.isfile(p):
                idx = file_index(open(p, encoding="utf-8").read())
                if idx:
                    out.append(f"-- {mod} --\n{idx}")
                break
    return "\n".join(out)[:5500]


def _compact_enum(block):
    """Enum as one line per variant: `Name { field: Type, ... }` - fits any size."""
    lines = block.split("\n")
    out = []
    i = 0
    while i < len(lines):
        m = re.match(r"^\s{4}([A-Z]\w*)\s*([{(]?)\s*$", lines[i])
        if not m:
            m = re.match(r"^\s{4}([A-Z]\w*)\s*([{(])(.*)$", lines[i])
        if not m:
            i += 1
            continue
        name, opener = m.group(1), m.group(2)
        fields = []
        if opener:
            close = "}" if opener == "{" else ")"
            j = i + 1
            depth = 1
            rest = m.group(3) if m.lastindex and m.lastindex >= 3 else ""
            if rest:
                fields.append(rest.strip())
            while j < len(lines):
                ln = lines[j].strip()
                depth += ln.count("{") + ln.count("(") - ln.count("}") - ln.count(")")
                if depth <= 0:
                    break
                if ln and not ln.startswith("#") and ln != close:
                    fields.append(ln.rstrip(","))
                j += 1
            out.append(f"{name} {opener} " + ", ".join(fields) + f" {close}")
            i = j + 1
        else:
            out.append(name)
            i += 1
    return "enum " + " | ".join(out)


def imported_types(text):
    """Full definitions of types imported by name - enum variants carry fields."""
    out = []
    seen = set()
    for m in re.finditer(r"use\s+crate::([A-Za-z0-9_:]+)::\{([^}]*)\}", text):
        mod = m.group(1)
        for n in m.group(2).split(","):
            n = n.strip().split(" as ")[0].strip()
            if not n or not n[0].isupper() or n in seen:
                continue
            seen.add(n)
            base = os.path.join(REPO, "crates/rivet-core/src", mod.replace("::", "/"))
            p = base + ".rs"
            if not os.path.isfile(p):
                p = os.path.join(base, "mod.rs")
            if not os.path.isfile(p):
                continue
            src = open(p, encoding="utf-8").read()
            lines = src.split("\n")
            offs = line_offsets(lines)
            for i, ln in enumerate(lines):
                if not re.match(
                    r"^\s*pub\s+(?:struct|enum)\s+" + re.escape(n) + r"\b", ln
                ):
                    continue
                is_enum = re.match(r"^\s*pub\s+enum\s", ln) is not None
                k = src.find("{", offs[i])
                close = scan_body(src, k) if k >= 0 else -1
                if close < 0:
                    break
                end = src.count("\n", 0, close)
                block = _strip_comments("\n".join(lines[i : end + 1]))
                out.append(_compact_enum(block) if is_enum else block)
                break
        if len("\n".join(out)) > 2600:
            break
    return "\n".join(out)[:2600]


def api_sheet(text):
    """Real field names and callable signatures, so bodies stop inventing APIs."""
    out = _def_blocks(text)
    for rel in EXTRA_FILES:
        p = os.path.join(REPO, rel)
        if os.path.isfile(p):
            out.extend(_def_blocks(open(p, encoding="utf-8").read()))
    lines = text.split("\n")
    cur = None
    for i, ln in enumerate(lines):
        mi = IMPL_RE.match(ln)
        if mi:
            cur = mi.group(1)
            continue
        if cur in KEEP_TYPES and re.match(
            r"^\s{0,8}(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:unsafe\s+)?fn\s", ln
        ):
            out.append(f"{cur}::{_sig(lines, i)}")
    for i, ln in enumerate(lines):
        if re.match(r"^pub(?:\([^)]*\))?\s+(?:const\s+|unsafe\s+)?fn\s", ln):
            s = _sig(lines, i)
            if s not in out:
                out.append(s)
    return "\n".join(out)[:7000]


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
        "- Never write todo!() or unimplemented!().\n"
        "- Error variants are either tuple-like `Error::Io(e)` or struct-like\n"
        "  `Error::LockHeld { path: x }`. Match the shape shown in IMPORTED TYPE\n"
        "  DEFINITIONS exactly; never pass a struct variant a bare argument.\n"
        "- Free functions in the API REFERENCE are called as functions, not methods.\n",
        "API REFERENCE (the real field names and method signatures; use exactly\n"
        "these and invent nothing that is not listed):\n" + api_sheet(text),
        "IMPORTED API (helpers that already exist and how to call them; use these,"
        " do not invent signatures): " + imported_api(text),
        "IMPORTED TYPE DEFINITIONS (exact variants and fields; construct them exactly):\n"
        + imported_types(text),
        "WORKED EXAMPLE (exactly how this crate hashes raw bytes):\n"
        "    let mut hasher = format.hasher();\n"
        "    hasher.update(&buf);\n"
        "    let id = hasher.finish().expect(\"hash collision\");\n"
        "    out.extend_from_slice(id.as_bytes());\n"
        "ObjectId::as_bytes() gives the raw digest; ObjectId::zero(format) is all-zero.",
        "SIGNATURE IS FROZEN: you cannot add a Result return. If a helper returns"
        " Result but the signature does not, use .expect(\"...\") on it.",
        "RETURN TYPE RULE: if the signature above does not return Result, the body"
        " must not use the ? operator.",
        "IMPORTS IN SCOPE:\n" + imports(text),
        "FUNCTION SIGNATURE (already written, do not repeat it):\n" + sig,
        "DOCUMENTED CONTRACT (this is the specification):\n" + doc,
        "NEARBY CONTEXT:\n" + ctx,
        "\nReturn the body for `" + stub["name"] + "` now.",
    ]
    return "\n\n".join(p for p in parts if p.strip())


# ---------------------------------------------------------------- model + tools
def ask(messages, timeout=420, temp=0.15):
    payload = {
        "model": MODEL,
        "stream": False,
        "think": False,
        "messages": messages,
        "options": {
            "temperature": temp,
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
    # Degenerate repetition guard: small quants sometimes lock onto one line and
    # emit it until num_predict runs out, which yields an unterminated body.
    # Reject the whole body so the caller retries at a higher temperature.
    run, prev = 1, None
    for raw in s.split("\n"):
        t = raw.strip()
        if t and t == prev:
            run += 1
            if run >= 3:
                return ""
        else:
            run = 1
        prev = t
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
    ap.add_argument("--only", default="")
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
        if args.only:
            todo_stubs = [s for s in todo_stubs if s["name"] == args.only]
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
                body, pt, et = ask(msgs, temp=0.15 if attempt == 1 else 0.45)
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
