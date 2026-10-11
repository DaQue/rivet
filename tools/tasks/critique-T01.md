You are reviewing a Rust reimplementation of Git ("rivet") in /home/davidq/Projects/rivet. Your
job is CRITIQUE, not repair.

This repository is READ-ONLY for you. Do not edit, write, or create any file. Do not run
`git add`, `git commit`, `git push`, `git remote`, or anything that changes state or touches a
network remote. If you think something must change, describe the change instead of making it.

## Context
- The library crates/rivet-core was generated as a SIGNATURE-ONLY scaffold: every function had a
  doc comment and a `todo!()` body. Frozen rule: signatures, types, lifetimes, generics and doc
  comments are not allowed to change; only bodies may be filled in. No new dependencies.
- The scaffold was implemented by a LOCAL, small LLM (Qwen 26.9B, IQ3_XXS quant, 32K context) run
  through Claude Code pointed at a local inference shim — deliberately as an experiment in what a
  weak local model can do with tight task specs. It is NOT written by a frontier model.
- Commits: `78ed248` is the untouched scaffold; `27e651e` implements crates/rivet-core/src/object/
  mod.rs and object/blob.rs; the working tree adds object/tree.rs (uncommitted at review time).
- The stated hard requirement is byte-for-byte on-disk format compatibility with real Git.

## What to review
1. `git show --stat 27e651e`, `git diff 78ed248..HEAD -- crates/rivet-core/src/object/` and
   `git diff HEAD -- crates/rivet-core/src/object/tree.rs` (the uncommitted work).
2. Read the implementation files in full, plus docs/ and tools/tasks/*.md for the intended spec.
3. Verify claims yourself where cheap: `git hash-object`, `git cat-file`, `cargo check`,
   `cargo test -p rivet-core`, `printf ... | sha1sum`.

## Judge specifically
- **Git byte-exactness.** Loose-object header `"<type> <len>\0"`; hash algorithm dispatch; tree
  payload = repeating `"<mode> <name>\0"` + 20 raw oid bytes (SHA-1) / 32 (SHA-256); tree sort
  order (name bytes, but a directory compares as if its name ended with `/`); mode strings octal
  with no leading zero (`100644`, `100755`, `120000`, `40000` written as `40000`, and note real
  git writes a tree/dir as `40000`, not `100644`); detection of truncated/short input; the
  SHA-1/SHA-256 oid length difference; anything that would make rivet produce a different hash
  than git for the same content.
- **Whether the frozen interface was actually respected** (signatures/doc comments unchanged).
- **Test quality**, the thing I care about most: would each test FAIL if the implementation logic
  changed? Name any tautological test, any assertion that only restates the implementation, and
  any critical invariant (sort order, NUL in header, declared-length checking, binary detection,
  malformed-entry rejection) that has no test at all. Quote file:line.
- **Rust correctness and robustness**: silent truncation, integer casts that lose data
  (`as` on lengths/offsets), panics where an error is required (indexing, `unwrap`, slicing),
  overflow, unbounded allocation from an attacker-controlled length, error type consistency with
  the crate's own error enum, iterator/alloc inefficiencies only where they matter.
- **What a strong human reviewer would reject in review**, even if tests pass.

## Output format
1. **Verdict** — 2 sentences: is this fit to build the rest of the port on, and at what risk?
2. **Findings**, worst first, each as: severity (Critical / Major / Minor / Nit) · `file:line` ·
   the concrete defect · why it is wrong · the fix. Cite real lines you have read; if you cannot
   cite a line, say "unverified".
3. **Test gaps** — the missing tests that would have caught the findings above.
4. **What is genuinely good** — name it plainly; do not pad.
5. **One-line grade** for the local model's output on this task: A–F plus the single biggest
   reason.

Rules of engagement: no praise inflation, no invented findings, and explicitly say "no defect
found in <area>" where you checked and found nothing. If a file is not what the task file in
tools/tasks/ said it should be, say so. Keep the whole report under 80 lines.
