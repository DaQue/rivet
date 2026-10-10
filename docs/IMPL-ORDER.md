# Rivet — implementation order (qwen phase)

Scaffold is signature-only: every function is a doc comment + `todo!()`. Each task below
replaces `todo!()` bodies with real code **inside the existing signatures**, adds unit tests,
and ends with `cargo check --workspace --all-targets` + `cargo test -p rivet-core` clean.

Run one task at a time (a 27B IQ3 model cannot hold the whole repo in its head):

    tools/qwen-impl.fish tools/tasks/T03.md T03

## Rules for every task (repeat these in the task file)

- Keep every signature, type, lifetime and doc comment exactly as scaffolded. If a signature is
  genuinely wrong, **stop and report it** — do not silently change it.
- No new dependencies (workspace deps are fixed in Cargo.toml).
- No `todo!()` left behind in the files the task names, no `unimplemented!()` anywhere.
- Deterministic code for deterministic work (parsing, hashing, byte layout). No LLM calls.
- Byte-exactness matters: this is a Git-compatible store, so round-trips and on-disk formats
  must match Git, not merely be self-consistent. Interop with real `git` is the acceptance test.
- Every task adds `#[cfg(test)]` unit tests that fail if the logic changes (not tautologies).

## Order (dependency-first, easiest first so failures surface early)

| # | Files | What it unblocks | Gate |
|---|-------|------------------|------|
| T01 | `hash.rs`, `object/blob.rs`, `object/tree.rs`, `object/mod.rs` | the object model | blob/tree round-trip vs `git hash-object` |
| T02 | `object/commit.rs`, `object/tag.rs` | commits, annotated tags | parse of a real `git cat-file commit` |
| T03 | `odb/loose.rs`, `odb/mod.rs` | loose object store | `git hash-object -w` then read by rivet |
| T04 | `index/mod.rs` | staging area (.git/index v2) | read a real index written by `git add` |
| T05 | `refs/*` | HEAD, branches, tags, reflog | resolve `refs/heads/main` written by git |
| T06 | `config/mod.rs`, `ignore/mod.rs` | repo config + .gitignore | parse a real `.git/config`, match patterns |
| T07 | `repo.rs`, `worktree/stat.rs`, `worktree/status.rs`, `worktree/add.rs` | init/open/status/add | `rivet status` agrees with `git status` |
| T08 | `worktree/checkout.rs`, `refs` wiring | branch switching | checkout a commit and diff trees |
| T09 | `revwalk.rs`, `diff/myers.rs`, `diff/unified.rs`, `diff/tree.rs` | log + diff | `rivet log`/`diff` byte-match git output |
| T10 | `merge/base.rs`, `merge/content.rs`, `merge/tree.rs`, `merge/state.rs` | merge | fast-forward + 3-way + conflict markers |
| T11 | `pack/delta.rs`, `pack/index.rs` | deltas + `.idx` v2 | read a real pack index |
| T12 | `pack/reader.rs`, `pack/mod.rs` | packfile read (OFS/REF delta, thin-pack) | **hardest piece** — clone gate |
| T13 | `pack/writer.rs` | packfile write | `git verify-pack` accepts our pack |
| T14 | `remote/url.rs`, `pktline.rs`, `sideband.rs`, `transport.rs` | wire vocabulary | pkt-line round-trip vs git's format |
| T15 | `remote/http.rs`, `remote/ssh.rs` | smart HTTP + ssh transports | `ls-remote` against GitHub |
| T16 | `remote/fetch.rs`, `remote/push.rs` | fetch/push | fetch a real remote into a real odb |
| T17 | `rivet-cli/src/main.rs` (+ `cmd/`) | wire the command surface | every subcommand runs, no `todo!()` |
| T18 | end-to-end | clone + push | clone a public GitHub repo; push branch+tag to a throwaway private repo with `git` agreeing |

**Needs explicit user go-ahead:** T18's push (public GitHub action) and anything that writes to a
remote. Cloning *from* a public repo is read-only and does not.
