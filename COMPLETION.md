# Rivet completion — definition B (workload gate)

Decided 2026-11-01. Rivet is **not** chasing command-name parity with git. It is done when it
can stand in for git on real work. Everything in GAPS.md is subordinate to this file.

## The gate

`tools/gate/workload.sh` — for each project in `tools/gate/projects.tsv`, clone the real repo
twice (once with `/usr/bin/git`, once with rivet) and require:

- **G1 object parity**: same `HEAD`, same `HEAD^{tree}`, same index (`ls-files -s`), same
  `for-each-ref` listing, `git fsck` clean on the rivet-clone, empty `status --porcelain`.
- **G2 workflow parity**: `tools/gate/workflow.sh` runs a fixed commit/branch/merge/rebase/
  stash/cherry-pick/tag/push sequence with pinned identities and dates under both binaries and
  diffs the resulting *state* (commit ids, tree hashes, refs, index, stash list, rev counts).
  Identical state or the gate is red.
- **G3 build parity**: the project's own test command runs to exit 0 **inside the rivet-cloned
  tree**. A byte-perfect object store with a broken worktree fails here.

## Exit criterion

All three projects green on G1, G2 and G3, on two consecutive runs, with no `-k` style skips.
That is the finish line for B. Not "GAPS.md is empty".

## Projects

See `tools/gate/projects.tsv`: a Go service (rigwatch), a Rust workspace (rivet itself), a Rust
GUI crate (lt3092egui). Three languages/toolchains, three real build systems, all local — the
gate must run offline.

## Scoreboard

`tools/gate/burndown.sh` appends `<date> <open §1> <open §2> <defects>` to `.claude/burndown.tsv`
and prints the delta. The relay runs it after every landed unit. Burn-down is the only progress
number that matters; commit count is not progress.

## Bookkeeping rules (enforced from block 20 on)

- GAPS.md is a **work list**, not a changelog. A unit must delete its bullet or promote it, never
  annotate it with what now works.
- Achievements go in CHANGELOG.md, one line, dated.
- No "Not done: …" clauses. If something remains, it stays a bullet; if it is done, it is gone.
- A bullet that describes behaviour which already works is a **defect in the list**, and removing
  it is a valid unit.
