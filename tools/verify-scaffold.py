#!/usr/bin/env python3
"""Verify the Rivet scaffold: batch-5 files, todo!() counts, cargo check + fmt."""
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path("/home/davidq/Projects/rivet")
SRC = ROOT / "crates/rivet-core/src"
OUT = pathlib.Path("/home/davidq/.hermes/cache/scratch/scaffold_report.txt")
lines = []


def say(s=""):
    lines.append(str(s))


def sh(cmd, cwd=ROOT):
    p = subprocess.run(cmd, cwd=cwd, shell=True, capture_output=True, text=True)
    return p.returncode, (p.stdout + p.stderr).strip()


# --- file inventory ---------------------------------------------------------
say("== files ==")
for p in sorted(SRC.rglob("*.rs")):
    say("%6d  %s" % (len(p.read_text().splitlines()), p.relative_to(SRC)))
say("%6d  crates/rivet-cli/src/main.rs" %
    len((ROOT / "crates/rivet-cli/src/main.rs").read_text().splitlines()))

# --- todo!() census (comment/string aware enough for this job) --------------
todo = 0
other_stub = 0
per_file = []
for p in sorted(SRC.rglob("*.rs")):
    txt = p.read_text()
    stripped = re.sub(r"//[^\n]*", "", txt)
    stripped = re.sub(r'"(?:[^"\\]|\\.)*"', '""', stripped, flags=re.S)
    n = len(re.findall(r"\btodo!\s*\(", stripped))
    o = len(re.findall(r"\bunimplemented!\s*\(|\bunreachable!\s*\(", stripped))
    if n or o:
        per_file.append((p.relative_to(SRC), n, o))
    todo += n
    other_stub += o
say()
say("== stubs ==")
say("todo!() total: %d" % todo)
say("unimplemented!/unreachable!: %d" % other_stub)
for rel, n, o in per_file:
    say("  %4d todo, %d other  %s" % (n, o, rel))

# --- non-todo bodies in the CLI --------------------------------------------
cli = (ROOT / "crates/rivet-cli/src/main.rs").read_text()
say()
say("== cli main.rs ==")
say("has todo!(): %s" % ("todo!(" in cli))
say("first 3 lines: %s" % " | ".join(cli.splitlines()[:3]))
say("remote mod declared in lib.rs: %s" %
    bool(re.search(r"^\s*pub mod remote;", (SRC / "lib.rs").read_text(), re.M)))
say("remote dir files: %s" %
    sorted(p.name for p in (SRC / "remote").glob("*.rs")) if (SRC / "remote").is_dir() else "MISSING")

# --- compile + format ------------------------------------------------------
say()
say("== build ==")
for label, cmd in (("cargo check --workspace --all-targets",
                    "cargo check --workspace --all-targets"),
                   ("cargo fmt --all --check", "cargo fmt --all --check")):
    rc, out = sh(cmd)
    tail = out.splitlines()[-25:]
    say("--- %s -> exit %d" % (label, rc))
    for t in tail:
        say("    " + t)
    say()

OUT.write_text("\n".join(lines) + "\n")
print("report written: %s (%d lines)" % (OUT, len(lines)))
