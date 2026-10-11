# Critique request: `rivet-core` tag object parser (T03)

You are reviewing a from-scratch Rust port of Git's object layer. Be a hard, specific reviewer.
**Read-only**: do not edit, write, commit, or push anything. Report findings only.

## What to review

- `crates/rivet-core/src/object/tag.rs` — the whole file: the annotated tag parser (`Tag::parse`),
  the writer (`Tag::to_bytes`), accessors, and the `#[cfg(test)]` module.
- `crates/rivet-core/tests/tag_git_differential.rs` — differential test against real git.
- `crates/rivet-core/tests/fixtures/tag/` — 11 raw tag-object *bodies* written by real `git`, plus
  `expected-object-ids.json` recording the object id git assigned to each.

Useful commands (allowed): `cargo test -p rivet-core`, `cargo check --workspace --all-targets`,
`git diff`, `git status`, `git hash-object -t tag`, `git cat-file`, `git mktag`, `sha1sum`.

## Ground truth and how it was established

Each fixture body came from real `git` (via `git mktag` / `git hash-object -t tag`). Every recorded
object id was independently re-derived as `sha1("tag <len>\0" + body)` and matched git exactly, so
the fixtures are byte-level truth, not approximations. Current state: `cargo test -p rivet-core`
passes 58 lib tests + 6 differential tests; `cargo check --workspace --all-targets` is clean;
`tag.rs` is the only modified source file.

The fixtures deliberately probe: PGP armour at line start vs mid-line; SSH armour; blank lines
before the armour marker; missing `tagger` header; an unknown header (`extra value`); missing blank
line / empty message; swapped `tag`/`tagger` order; no tagger and no blank line; and an unknown
`type` value.

## Three bodies git accepts that this port deliberately rejects

These were pinned as *documented deviations* rather than bugs, on the reasoning that the port's
contract requires `tagger` and requires the blank line that terminates the header block:

- case `D`: no `tagger` header, blank line present → `Error::MissingHeader`
- case `J`: no `tagger` header and no blank line → `Error::MalformedObject`
- case `F`: no blank line, no message → error

Judge this honestly: call it a defensible contract if you believe it is one, or a faithfulness defect
if a Git-compatible parser should accept what git accepts. Say which, and why.

## What I want back

1. **Defects**, ordered by severity, each with `file:line`, what breaks, and a concrete repro
   (a body, a `git` command, or a test).
2. **Byte-exactness verdict**: is there any input for which `to_bytes()` would not reproduce the
   original bytes (and therefore change the object id)? Name it.
3. **Armour/signature boundary**: are the marker rules right (line-start only, message keeps the
   newline before the marker, mid-line markers stay message text)? Any counterexample?
4. **Test quality**: do the tests in `tag.rs` and the differential test actually fail when behaviour
   changes, or are any of them tautological/overfitted to the implementation? Name the weak ones.
5. **Rust quality**: panics/`unwrap` on untrusted input, allocation hot spots, lifetime/borrow
   smells, anything that would break on adversarial input (huge headers, embedded NULs, CRLF,
   non-UTF-8, missing final newline).
6. **Top 5 fixes** before this can be called a faithful port, in order.

Be concrete. Quote code. If something is fine, say so briefly and move on — do not pad.
