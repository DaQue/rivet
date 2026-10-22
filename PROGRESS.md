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
Frozen-grammar gaps (the scaffold's flag set differs from git's): several `rev-parse`/`ls-tree`/`cat-file` flags.

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

## Porcelain completeness I (2026-10-22)

Landed (each has a differential scenario in `tools/difftest/scenarios.sh`; "twin" = real git and rivet compared on
stdout, stderr, exit code, refs, index, worktree, reflogs, pseudo-refs and `fsck`):

| item | status | scenario |
|---|---|---|
| 1. `log -1` / `-n1` / `-N` | landed (argv rewrite in `main.rs`, also `reflog -N`) | `s_log_forms` |
| 2. `log --format/--pretty/--date` | landed: `%H %h %T %t %P %p %s %b %B %f %an %ae %aN %aE %ad %aD %ar %ai %aI %as %at` (+ `c*`), `%d %D %n %%`, `%xNN`; `oneline short medium full fuller format: tformat:`; `--date=default iso iso-strict relative short raw unix rfc2822` | `s_log_forms` |
| 3. `show --stat/--name-only/--name-status` (+ `--format/--pretty/--date`) | landed | `s_log_forms` |
| 4. `verify-pack -v` | landed (`-v` was shadowed by the subcommand's long-only flag); output already matched git's columns | `s_verify_pack_v` |
| 5. `revert` | landed: single commit, `--no-commit`, `--abort`, `--continue`, `--no-edit`, resolvable conflict state (`REVERT_HEAD`, `MERGE_MSG`, stages, markers), `Reapply "…"` | `s_revert`, `s_revert_options`, `s_revert_conflict` |
| 6. `cherry-pick` | landed: commits, `A..B` ranges, `--no-commit`, `--abort`, `--continue`, conflict state | `s_cherry_pick`, `s_cherry_pick_options`, `s_cherry_pick_conflict`, `s_cherry_pick_conflict_nocommit`, `s_cherry_pick_delete_conflict`, `s_cherry_pick_empty_and_merge` |
| 7. `stash` push/list/show/pop/apply/drop (+ clear) | landed on the real `refs/stash` + reflog layout; stash commit ids are byte-identical to git's | `s_stash`, `s_stash_stack`, `s_stash_conflict` |
| 8. `reflog [show] [<ref>]`, `--oneline`, `-n`, `-N` | landed | `s_reflog` |
| 9. `clean -n/-f/-d/-x` (+ `-X`, `-q`, pathspecs, `-ff`) | landed, honours `.gitignore`; `-n` removes nothing | `s_clean`, `s_clean_edge` |
| 10. `mv` | landed: files, directories, several sources, `-f -n -k -v`, from subdirectories | `s_mv` |
| 11. `gc`, `count-objects -v/-H` | landed | `s_gc`, `s_gc_prune`, `s_gc_reflog`, `s_gc_after_cruft`, `s_count_objects` |

Also: `status` now reports a stopped cherry-pick/revert and git's wording for unmerged paths outside a merge;
`Odb::write_raw` no longer writes an object that a pack already holds (git does not either; it showed up in
`count-objects`); `rivet_core::merge::restore_to_commit` was factored out of `abort_merge`; new rivet-only tests in
`crates/rivet-cli/tests/{sequencer,maintenance}.rs`.

Public-API note: the task required new flags on `LogArgs`/`ShowArgs`/`VerifyPackArgs` and new subcommands, so the frozen
CLI grammar grew; no existing signature or doc comment in `rivet-core` was changed (additions only: `ops::format_date_style`,
`Repository::worktree_tree`, `gc::*`, `merge::restore_to_commit`).

### Known gaps from this block (all refuse or say so; none silently wrong)
- **cherry-pick/revert**: a multi-commit sequence is rehearsed on trees first and *refused before any write* if it would
  stop on a conflict or an empty commit (exit 1) — there is no `.git/sequencer` resume state, so `--skip`/`--quit`, `-m`,
  `-x`, `-e`, `--allow-empty`, `--keep-redundant-commits` and signing are not implemented. A single empty pick exits 1 and
  leaves nothing behind (git leaves `CHERRY_PICK_HEAD`). `revert` on a terminal requires `--no-edit` (no editor support);
  reverting a root commit is refused. Conflict diagnostics: `diff` of conflicted paths is still only `* Unmerged path`.
- **stash**: `-u/-a/-k`, pathspecs, `apply --index`, `branch/create/store/save` are refused.
- **reflog**: `expire/delete/exists`, several refs, `--date`, `-g`-style selectors other than `<ref>@{N}` output.
- **clean**: `-i`, `-e`; output paths for pathspecs outside the cwd are `../`-relative but only tested for sibling dirs;
  nested repositories are skipped silently (as git does without `-ff`).
- **mv**: `--sparse`.
- **gc**: writes one *undeltified* pack (valid but larger than git's), no cruft packs (young unreachable objects stay
  loose; git's cruft packs and their `.mtimes` are read and honoured), no commit-graph, no `worktree prune`/`rerere gc`,
  `--keep-largest-pack`, `--cruft`. `gc --auto` uses git's loose-object estimate but not its hooks/daemonising.
- **count-objects**: no `alternate:` lines.
- **log/show**: unsupported `%`-placeholders (`%C…`, `%w`, `%N`, `%G*`, `%(trailers)`) are left verbatim as git leaves unknown
  ones; `--date=human|format:`; `--decorate` on block layouts (decorations are available through `%d`/`%D`); `show` of a merge
  prints no patch/name-only (git prints a combined diff), but its `--stat` is against the first parent as in git.
- **Found while testing, not fixed (outside this block's list)**: `status --untracked-files=all` descends into an untracked
  nested repository (git prints `nested/`); `checkout -- .` with an unmerged path exits 0 (git: `error: path 'f' is
  unmerged`, exit 1); `git stash` pathspec/`-u` states above.

### Gates (2026-10-22, after the last commit of the block)
- `cargo fmt --check`: clean (exit 0)
- `cargo clippy --workspace --all-targets -- -D warnings`: clean (exit 0)
- `cargo test --workspace`: 193 passed, 0 failed, 0 ignored (includes `difftest` running every scenario, and the new
  `sequencer` and `maintenance` tests)
- `bash tools/difftest/scenarios.sh`: 74 PASS, `failures: 0` (exit 0)
