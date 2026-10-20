# PROGRESS

## Phase 1 — done
All 19 CLI stubs are implemented (`grep -rn "todo!()" crates/` → stale doc comments only).
Shared logic lives in `rivet-core`: `ops` (identity, HEAD moves + reflogs, checkout/reset/restore/merge),
`remote_ops` (refspecs, remote config, fetch/push), `fsck`, `pack::verify`, real `CommitWalker`.

## Phase 2 — differential harness (`tools/difftest/scenarios.sh`, `lastfail.sh`)
17 scenarios, real git vs rivet, comparing stdout/stderr/exit codes and repo state (refs, index, worktree, reflogs, config, FETCH_HEAD, packed-refs, fsck):
basic_commit, branch_ops, switch_checkout, merge_ff_and_true, merge_conflict, log_show_diff, tags, reset, restore,
config, check_ignore, fsck_verify_pack, fsck_missing, cross_read, remote_fetch_push, pull, clone — all PASS.

## Phase 3 — bugs found and fixed by the tests
init layout/config; config editing (add/remove/sections never matched); `merge_bases` placeholder;
tz sign; root-commit diffs; `--stat`; hunk function context; `CommitWalker`/`WalkOptions` no-ops;
pack `iter_entries` start offset; `RemoteUrl::parse` (absolute paths, scp-style); `push_is_fast_forward`/`check_updates`;
`Config::get_all` order; `Odb::oids` O(n²).

## Known gaps (documented, bail with a message)
commit -S/-C/-c/--author/--dry-run; log --graph; merge --squash/octopus/-X; pull --rebase; switch/checkout --merge;
reset --keep/--merge; push --mirror/--force-with-lease/--atomic; clone --mirror/--depth; fetch --depth.
Frozen-grammar gaps: `log --format`, `show --stat`, `verify-pack -v` (use `--verbose`).
