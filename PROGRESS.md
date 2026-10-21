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

## Status (2026-10-21)

Diff engine: Myers (`xdl_split`/`xdl_cleanup_records`) plus change compaction and the indent heuristic match git on 600/600 random
pairs. Histogram diff follows `xhistogram.c` without `xdl_trim_ends` and matches git on ~98% of random pairs (294/300); the
residual divergence is a tie-break on repeated lines that was not pinned down (trimming ends made it worse, 270/300).

`cargo test --workspace`: 178 passed, 0 failed. `tools/difftest/scenarios.sh`: every scenario passes, named and randomized
(random_1..8), including ssh and http transports (fake ssh, `git http-backend`), sha256, delta-heavy packs, cat-file batch,
rename-aware merges, log path limiting.

Fixed in this block: switch/merge refusal rules (staged deletes, combined dirty+untracked report), `Keep` entries in checkout plans,
`cat-file --batch/--batch-check/--batch-all-objects`, `diff.renames`/`status.renames`/`merge.renames`, rename/delete and
rename/rename merges, ssh thin-pack, http redirects (follow the new location for the whole session).

Notes against .claude/PRIORITIES.md: real git (2.56) *does* detect renames in plain `diff`/`status`/`log` by default and honours
`diff.renames=false`; the harness (`s_renames`, `s_rename_config`) proves rivet matches that.
