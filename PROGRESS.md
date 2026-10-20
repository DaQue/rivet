# PROGRESS

## Phase 1 — done
All 19 CLI stubs are implemented (`grep -rn "todo!()" crates/` → stale doc comments only).
Shared logic lives in `rivet-core`: `ops` (identity, HEAD moves + reflogs, checkout/reset/restore/merge),
`remote_ops` (refspecs, remote config, fetch/push), `fsck`, `pack::verify`, real `CommitWalker`.

## Phase 2 — differential harness (`tools/difftest/scenarios.sh`, `lastfail.sh`; also run by `cargo test -p rivet-cli --test difftest`)
31 scenarios, real git vs rivet, comparing stdout/stderr/exit codes and repo state (refs, index, worktree, reflogs, config, FETCH_HEAD, packed-refs, fsck). All PASS:
basic_commit, branch_ops, switch_checkout, merge_ff_and_true, merge_conflict, merge_edge, log_show_diff, tags, reset, restore, config,
check_ignore, fsck_verify_pack, fsck_missing, corrupt, corrupt_pack, cross_read, remote_fetch_push, push_cases, pull, pull_merge,
fetch_prune_tags, clone, packed_refs, status_variants, plumbing, sha256, odd_names.

## Phase 3 — bugs found and fixed by the tests
init layout/config (+sha256 extension order); config editing (add/remove/sections never matched); `merge_bases` placeholder;
tz sign; root-commit diffs; `--stat`; hunk function context; `CommitWalker`/`WalkOptions` no-ops; pack `iter_entries` start offset;
`RemoteUrl::parse` (absolute paths, scp-style); `push_is_fast_forward`/`check_updates`; `Config::get_all` order; `Odb::oids` O(n^2);
local transport (unknown haves, ref deletion wrote a null id); push pack pruning; `Object::parse` assumed SHA-1 (broke every SHA-256 commit);
rev-parse peeling (`^{}`, `^{commit}`); mode-only change reported as typechange (test fixed — git is the oracle); diff output
(mode-change `index` line, missing newline before `\ No newline`, binary-file line); `rm` safety checks; path quoting (`core.quotePath`);
`ls-files -u`; `status` upstream tracking, unmerged section, `add/rm` hint; worktree config parse panics.

## Known gaps (documented, bail with a message)
commit -S/-C/-c/--author/--dry-run; log --graph; merge --squash/octopus/-X; pull --rebase; switch/checkout --merge;
reset --keep/--merge; push --mirror/--force-with-lease/--atomic; clone --mirror/--depth; fetch --depth.
`diff` of conflicted paths prints only `* Unmerged path` (no `diff --cc` combined diff).
Frozen-grammar gaps (the scaffold's flag set differs from git's): `log --format`, `show --stat`, `verify-pack -v` (use `--verbose`), several `rev-parse`/`ls-tree`/`cat-file` flags.
Old note: `log --format`, `show --stat`, `verify-pack -v` (use `--verbose`).

## Current failing at HEAD (11:35 after the 11:04 session-limit stop)

`cargo test -p rivet-cli --test difftest` → 2 of the randomized scenarios fail; every named scenario passes.

- `s_random_7`: `switch -q br2` aborts with "Your local changes to the following files would be overwritten by checkout: c.txt" where real git switches fine and carries the change. Everything after that cascades onto the wrong branch. Also `restore b.txt` / `restore --staged b.txt` on a path absent from HEAD exit 0, while git errors with `pathspec 'b.txt' did not match`.
- `s_random_8`: `switch -q br0` → `fatal: No such file or directory (os error 2)`, exit 128 — a checkout read of a missing path. Same `restore` pathspec divergence follows.

Both are in the switch/checkout path (`crate::worktree::checkout`), i.e. exactly what was mid-flight when the run was cut off. Retained repro dirs with full transcripts and `state.diff`:
`/home/davidq/.hermes/cache/scratch/difftest.MTsMun` (scenario 7) and `difftest.nJJkPw` (scenario 8).
