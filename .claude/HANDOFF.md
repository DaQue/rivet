# rivet — autonomous completion run

You are working autonomously in `/data/rust/rivet`, a from-scratch Rust reimplementation of
`git` that must match real Git's on-disk format and command semantics.

Do not stop to ask questions. Do not ask for confirmation. Work the checklist below in order,
committing after each green milestone. Keep `PROGRESS.md` updated as you go so a human can
read status at any moment.

## Current state (verified)

- Workspace: `crates/rivet-core` (library) and `crates/rivet-cli` (binary `rivet`).
- `cargo check --workspace` is clean.
- `rivet-core` has **zero** `todo!()` bodies (1160 fns). The SSH transport, fetch, push,
  sideband store, and local push pack install were just implemented.
- `rivet-cli` has **19** `todo!()` command handlers (53 fns total). These are the surface gap.
- A clean compile is NOT evidence of correctness. Several core functions may compile and still
  produce wrong repository state. That is the real target of this run.

## Phase 1 — implement the 19 CLI stubs

Replace every `todo!()` with a real implementation that calls into `rivet-core`:

- `crates/rivet-cli/src/cmd/branching.rs`: `run_branch`, `run_switch`, `run_checkout`, `run_merge`
- `crates/rivet-cli/src/cmd/remote.rs`: `run_remote`, `run_fetch`, `run_pull`, `run_push`
- `crates/rivet-cli/src/cmd/history.rs`: `run_log`, `run_show`, `run_diff`
- `crates/rivet-cli/src/cmd/plumbing.rs`: `run_verify_pack`, `run_fsck`
- `crates/rivet-cli/src/cmd/setup.rs`: `run_clone`, `run_config`
- `crates/rivet-cli/src/cmd/snapshot.rs`: `run_restore`, `run_reset`
- `crates/rivet-cli/src/cmd/ignore.rs`: `run_check_ignore`
- `crates/rivet-cli/src/cmd/tag.rs`: `run_tag`

Match the argument parsing, output text, and exit codes of the real `git` command in each case.
Where a core API is missing, add it to `rivet-core` rather than reimplementing logic in the CLI.
After Phase 1, `grep -rn "todo!()" crates/` must return nothing but stale doc comments.

## Phase 2 — differential testing against real Git

Build the binary (`cargo build --workspace`) and run scenarios in throwaway directories
(`mktemp -d`) where the same operation is performed by `/usr/bin/git` and by `rivet`, then
compare resulting repository state. Cover at minimum:

1. `init` → `add` → `commit` → compare `.git` structure, refs, HEAD, index, and objects
   (`git cat-file -p`, `git ls-tree -r`, `git rev-parse HEAD` on both repos).
2. Cross-read: `rivet` must read a repo created by `git`; `git fsck` must accept a repo
   created by `rivet` with no errors and no dangling surprises.
3. `branch` / `checkout` / `switch` / `merge` (fast-forward, true merge, conflict case).
4. `log`, `show`, `diff` output equivalence on the same history.
5. `tag` (lightweight and annotated), `restore`, `reset` (soft/mixed/hard).
6. `remote add`, `fetch`, `pull`, `push` against a local bare repo created by real git
   (file:// and plain path), including tracking refs, packed-refs, and reflogs.
7. `clone` from a git-created bare repo; verify the clone is usable by real git.
8. `verify-pack` and `fsck` agreeing with real git on the same pack files.

Use `git fsck --strict`, `git verify-pack -v`, `git show-ref`, and manual byte comparison of
`pack-*.idx`/`pack-*.pack` where relevant. Treat any divergence as a bug.

## Phase 3 — fix what the tests expose

Differential failures are the priority. Also audit for silent placeholders: functions that
return `Ok(())` without doing work, `unwrap_or_default()` where an error belongs, ignored
`let _ =` results on IO, and validation that is documented but not performed. Fix them.

## Rules

- English only in all files, commits, and output.
- Commit after each green milestone. Use two separate commands: `git add -A`, then
  `git commit -m "..."`. Never amend or rewrite history.
- Do not modify `docs/IMPL-ORDER.md`.
- Never leave the tree in a non-compiling state across a commit; run `cargo check --workspace`
  before committing, and `cargo test --workspace` where tests exist.
- Prefer real `git` behavior over convenience when they disagree.
- Keep `PROGRESS.md` current: what is done, what is verified, what remains.
