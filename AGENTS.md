# AGENTS.md — rivet

Rivet is a from-scratch **Git-compatible** implementation in Rust (core + CLI). The repo is a
signature-first scaffold being filled in one task at a time. Working tree is normally clean on
`master`; every task ends as an uncommitted diff of the files that task names.

Design source of truth: `/home/davidq/notes/projects/rivet-plan.md`.
Implementation order and task list: `docs/IMPL-ORDER.md`, briefs in `tools/tasks/`.

## Hard rules (apply to every task)

1. **Real `git` is the oracle, not your intuition.** Byte layouts, object ids, index/ref/pack
   formats must match Git. When a test and `git` disagree, git is right — fix the test and say so.
2. **Do not change public API.** Signatures, types, lifetimes, visibility and `///` / `//!` doc
   comments are frozen. If a signature is genuinely wrong, stop and report it instead of editing it.
3. **No new dependencies.** Workspace deps are fixed in `Cargo.toml`. `serde_json` is not available.
4. **No `todo!()` / `unimplemented!()` left behind** in the files the task names.
5. **Deterministic code only** for parsing/hashing/byte layout. No network calls, no LLM calls.
6. **Bytes, not strings**, for anything read from disk: `bstr::BString` / `&BStr` / `&[u8]`.
   `String` only for text you construct yourself.
7. **Never panic on repository data** — every parser returns `Result<_, Error>` with a specific
   variant. `anyhow` is CLI-only; core uses the single `Error` enum.
8. **Tests must fail if the logic changes.** No tautologies, no deleted/ignored/weakened tests.
9. **Scope discipline:** edit only the files the task names. `git status --short` at the end must
   show exactly those paths — no scratch files, no commits, no reformatting of untouched code.

## Read discipline (this is where previous runs failed)

A past run made 96 `Read` calls, 0 edits, and never wrote a byte. Therefore:

- Read only the line ranges the brief gives you, with `offset` + `limit`, never a whole file.
- Make your **first edit by tool call #8**.
- Never inspect files with shell (`cat`, `sed`, `grep`, `head`, `tail`). Shell is for `cargo`,
  `git`, and compiled probes only. (`grep -c 'todo!()' <file>` as a completion check is fine.)
- Do not re-read a region you have already read. Keep it, act on it.

## Build and verify

```bash
cargo check --workspace --all-targets
cargo test -p rivet-core
cargo test -p rivet-cli
cargo fmt
```

A run that ends with zero shell commands has failed. Never end with a red build, a
half-implemented file, or functions still on `todo!()`.

## Report at the end

What you changed per file, which caveat you implemented literally, what your tests would catch,
the exact pass/fail/ignored counts, and anything ambiguous in a doc comment. If you could not
finish, say exactly what is red and why — do not claim success you did not verify.

## Current work

`tools/tasks/T04.md` — fix the tag signature boundary and make the differential test compare
against recorded git object ids. Which path to work next is decided by the user, not by you:
finish the named task and stop.
