# Rivet scaffold brief (signature-only)

Read `/home/davidq/notes/projects/rivet-plan.md` first — it is the design source of truth
(§2.1 hash strategy, §3 architecture, §4 phases, §5 robustness, §7 commands, §8 milestones).

## The task, verbatim from the user

> scaffold the app. No functions written just their signature types and life times with a
> comment on what it should do and any caveats. Body should include the `todo!` macro

## Hard rules

1. **Every function body is exactly `todo!()`.** No logic, no `unimplemented!()`, no partial
   implementations, no `Default` impls that build a real value (write `todo!()`), no
   `unreachable!()`. One-line bodies only.
2. **Signatures carry the design.** Types, references, lifetimes, generics, `where` clauses.
   Use explicit lifetimes wherever data is borrowed (`Blob<'a>`, `fn parse(data: &'a [u8]) -> Result<Self>`).
   Owned returns where the value outlives the input (`Vec<u8>`, `BString`, `ObjectId`).
3. **Every public item has a doc comment** — what it does, and the caveats that will bite the
   implementer. Use these sections where they apply:
   - `# Caveats` — format quirks, Git-version behaviour, invariants, ordering rules.
   - `# Errors` — which `Error` variants, and when.
   - `# Panics` — only if a caller-visible invariant can be violated; otherwise omit.
   Start the doc comment with the phase, e.g. `/// Phase 10 — read a v2 pack index.`
4. **No new dependencies.** `Cargo.toml` files are already correct; do not edit them.
   Available: `thiserror`, `flate2`, `sha1-checked`, `sha2`, `bstr`, `globset`, `jiff`, `ureq`
   (core) and `clap`, `anyhow`, `rivet-core` (cli).
5. **It must compile.** `todo!()` diverges, so signature-only code type-checks. Run
   `cargo check --workspace` and fix *signature* errors (missing types, wrong lifetimes,
   unused imports). Never fix a compile error by writing an implementation.
6. **Structs, enums and type aliases ARE expected** — fields are type declarations, not bodies.
   Function and trait-method bodies are the only things that must be `todo!()`.
7. **Never panic on repository data** (§5). Every parser returns `Result<_, Error>` with a
   specific variant. `Error` is `thiserror`-derived, no `Box<dyn Error>`, no `anyhow` in core.
8. **Bytes, not strings.** Paths, ref names and object payloads are `bstr::BString` / `&BStr` /
   `&[u8]`. Never `String` for anything that comes off disk. `String` is only for human-facing
   text you construct yourself (and `&str` for static literals).
9. **No `async`.** The core is synchronous (§3).
10. Rust 2024 edition. `cargo fmt` clean. Do not add `#[allow(...)]` beyond what `lib.rs` already has.

## Shared vocabulary — use these exact names in every batch

```rust
rivet_core::error::{Error, Result}
rivet_core::hash::{ObjectFormat, ObjectId, ObjectHasher}
rivet_core::object::{ObjectKind, Object, AnyObject, Blob, Tree, TreeEntry, EntryMode, Commit, Signature, Tag}
rivet_core::odb::{Odb, LooseObjectStore}
rivet_core::pack::{PackFile, PackIndex, PackWriter, EntryKind, apply_delta, index_pack}
rivet_core::refs::{RefStore, RefTarget, RefName, ReflogEntry}
rivet_core::index::{Index, IndexEntry, IndexExt, Stage}
rivet_core::config::Config
rivet_core::ignore::{IgnoreStack, Pattern, IgnoreSource}
rivet_core::worktree::{Worktree, Status, StatusEntry, ChangeKind, CheckoutPlan}
rivet_core::diff::{DiffAlgorithm, Hunk, HunkLine, LineKind, TreeDiff, FileDiff, DiffOptions}
rivet_core::merge::{MergeBase, MergeOutcome, Conflict, ConflictKind}
rivet_core::remote::{PktLine, Transport, HttpTransport, SshTransport, LocalTransport, RefUpdate, FetchOutcome}
rivet_core::repo::Repository
```

Fixed decisions that keep the batches consistent:

- `pub type Result<T> = std::result::Result<T, Error>;` in `error.rs`.
- `ObjectId` is an **enum** `{ Sha1([u8; 20]), Sha256([u8; 32]) }` (plan §2.1), `Copy + Eq + Hash + Ord`.
  Nothing anywhere may hard-code 20 bytes or 40 hex chars.
- `ObjectFormat { Sha1, Sha256 }` with `len()`, `hex_len()`, `hasher()`, `from_config_value()`, `as_str()`.
- Commits/tags/objects borrow their payload: `Blob<'a> { pub data: &'a [u8] }` etc. The *transaction*
  types (`Object`, `Index`, `Config`, `PackIndex`) own their data.
- Timestamps are lossless: `Signature { name: BString, email: BString, when: i64, tz_offset_minutes: i32 }`
  (Git stores `<unix> <+hhmm>`); offer `jiff` conversion helpers rather than storing a `jiff` type.
- Errors are the single `Error` enum — add variants to it rather than introducing new error types.
- `lib.rs` starts with `#![allow(dead_code)] // scaffold stage: everything is todo!() + unused` so the
  scaffold is warning-clean. Leave that in place.

## Module inventory (already created; nothing else exists)

```
crates/rivet-core/src/
├── lib.rs          # module tree + pub use re-exports
├── error.rs        # Error enum + Result alias
├── hash.rs         # ObjectFormat, ObjectId, ObjectHasher
├── object/         # Blob, Tree, Commit, Tag + ObjectKind/Object/AnyObject
├── odb/            # object database: loose + packed dispatch
├── pack/           # packfile, .idx v2, delta apply, pack write/index
├── refs/           # loose refs, packed-refs, symbolic, locks, reflog
├── index/          # DIRC v2
├── worktree/       # status, checkout + safety, add/rm
├── ignore/         # .gitignore semantics
├── diff/           # Myers, tree diff, unified output
├── merge/          # merge-base, 3-way, conflicts
├── config/         # Git INI
├── remote/         # pkt-line, HTTP/SSH/local transport, negotiation
└── repo.rs         # Repository glue (open / discover / init + accessors)
crates/rivet-cli/src/main.rs   # clap CLI covering plan §7
```

`repo.rs` is the one file not named in plan §3 — it is the glue every CLI command needs.
Keep it thin: open/discover/init plus accessors (`odb()`, `refs()`, `index()`, `config()`,
`worktree()`, `ignore()`), and the operation entry points the CLI calls
(`add`, `commit`, `status`, `log`, `checkout`, `switch`, `branch`, `tag`, `diff`, `merge`,
`clone`, `fetch`, `pull`, `push`).

## Batch boundaries (one agent run per batch)

- **Batch 1** — `lib.rs`, `error.rs`, `hash.rs`, `object/*`.
- **Batch 2** — `odb/*`, `pack/*`.
- **Batch 3** — `refs/*`, `index/*`, `config/*`, `ignore/*`.
- **Batch 4** — `worktree/*`, `diff/*`, `merge/*`, `remote/*`, `repo.rs`.
- **Batch 5** — `crates/rivet-cli/src/main.rs`.

In every batch: read the files already written by earlier batches so the shared vocabulary
matches exactly, write your files, then run `cargo check --workspace` and `cargo fmt`, and fix
signature-level errors only. Report: files written, one-line summary each, `cargo check` result,
and any place where you had to guess a type name that a later batch must honour.
