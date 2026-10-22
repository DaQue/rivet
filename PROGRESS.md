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
commit -S/-C/-c/--author/--dry-run; log --graph; merge --squash/octopus/-X; pull --rebase; switch/checkout --merge (landed 2026-10-22, see below);
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

## Porcelain completeness II (2026-10-22)

Every item has a differential scenario in `tools/difftest/scenarios.sh` (real git vs rivet on stdout, stderr, exit code,
refs, index, work tree, reflogs, pseudo-refs, `fsck`). New scenarios are listed with their item.

| item | status | scenario |
|---|---|---|
| B1 `status -uall` with a nested repo | landed: an untracked directory holding a `.git` is one `dir/` entry in every mode | `s_status_nested` |
| B2 `checkout -- <unmerged path>` | landed: `error: path 'f' is unmerged`, exit 1, other paths still restored | `s_checkout_unmerged` |
| B3 `diff` of a conflicted path | landed as git's combined `diff --cc` (not a plain conflict diff: that is what git prints), incl. `-U<n>`, name-only/name-status, `* Unmerged path` + stage-2 diff for modify/delete, `--cached` listing; new `rivet_core::diff::combined` | `s_diff_conflict`, `s_diff_conflict_shapes` |
| S1 `stash push -u/-a/-k`, pathspecs, `apply/pop --index` | landed on git's exact object layout (untracked commit as 3rd parent, empty one with `-u`), HEAD reflog/ORIG_HEAD rules; `stash apply` keeps the prior index; untracked restore failure reports like git | `s_stash_untracked`, `s_stash_keep_index`, `s_stash_pathspec`, `s_stash_index` |
| S2 `clean -i`, `clean -e` | landed: git's menu (clean, filter, select by numbers, ask each, quit, help, prefixes, `?`), column layouts, `-e` incl. with `-x/-X` | `s_clean_interactive`, `s_clean_exclude` |
| S3 `gc` deltas | landed: `pack-objects`' search (type/name-hash/size sort, window 10, depth 50, `try_delta` size rules, best-base move, reuse of existing deltas, `pack.window/depth`) and a port of `create_delta`; the delta structure (ids, types, sizes, depth, base) is identical to `git gc` on the scenarios. Cruft packs: **not done** (see gaps) | `s_gc_delta`, `s_gc_delta_shapes` (+ unit tests in `pack::deltify`) |
| Q1 `rev-list` `--count --max-count --all --reverse A..B A...B ^X --first-parent --objects` | landed (`--objects` included: names, edge trees, tags, `--reverse` order) | `s_rev_list` |
| Q2 `show-ref` `--heads --tags -d --verify --head -s -q` | landed | `s_show_ref` |
| Q3 `for-each-ref --format` | landed: `refname[:short,:strip,:lstrip,:rstrip]`, `objectname[:short[=N]]`, `objecttype`, `objectsize`, `symref`, `HEAD`, `*`-peeled atoms, `--sort`, `--count`, patterns; `:short` uses git's strict ambiguity rule | `s_for_each_ref` |
| Q4 `merge-base` `--all --is-ancestor` | landed (two commits) | `s_merge_base` |
| Q5 `describe` `--tags --always --long --abbrev --exact-match` | landed: annotated > lightweight, newer tagger date wins, 10-candidate limit, depth = commits not reachable from the tag, git's three failure messages | `s_describe` |
| Q6 `shortlog -s -n -e -c` | landed | `s_shortlog` |
| R1 `rebase` | landed: `<upstream> [<branch>]`, `--onto`, `--continue/--skip/--abort`, merges flattened, patch-equivalent commits dropped (`warning: skipped previously applied commit`), reuse of unchanged commits, `rebase-merge/` state, `status` while stopped, reflogs byte-identical | `s_rebase_basic`, `s_rebase_onto`, `s_rebase_cherry_and_merges`, `s_rebase_dirty`, `s_rebase_conflict`, `s_rebase_skip_abort`, `s_rebase_multi_conflict` |
| P5a `grep` (work tree, `--cached`, `<rev>`) | landed with an own POSIX BRE/ERE matcher (no new crates): `-e -i -w -v -F -E -G -n -l -L -c -q -h -H -a -I -A -B -C`, pathspecs, subdirectories, binary files | `s_grep` (+ regex unit tests) |
| P5b `blame` | landed: `[rev] [--] file`, `-L` (N,M / N,+K / N,-K), `-e -s -l -f`, merges, whole-file renames, root `^` boundary | `s_blame`, `s_blame_edits` |
| P5c `apply`, `format-patch`, `am` | landed: `apply [--check --index --cached -R -p<n>]` (offsets, no fuzz), `format-patch [-N] [<since>\|a..b] [-o --stdout --root -n --start-number --subject-prefix --signature]`, `am [mbox…] / --continue / --skip / --abort` with a resumable `.git/rebase-apply`, `status` text for an am session | `s_apply`, `s_format_patch`, `s_format_patch_wrap`, `s_am` |

Other changes: `Repository::merge_into_worktree` no longer reports a path as dirty when the index and work tree already
hold exactly what the merge would write (git's rule; needed by `stash apply`); `rivet_core::diff::unified::format_stat_width`
(new) backs `format-patch`'s 72-column stat; `PackWriter::write_ofs_delta`/`last_offset` and
`PackedObjectStore::{oids_by_offset, stored_delta}` are new items. No existing public signature or doc comment in
`rivet-core` was changed.

### Known gaps from this block (all refuse, or are listed here; none silently wrong as far as the scenarios show)
- **gc**: no cruft packs (young unreachable objects stay loose, as before; git's cruft packs are still read); the object
  list follows `rev-list --objects` order approximately (commit date order, trees depth first) and the write order is
  commits/tags, trees, blobs with bases first, not git's `compute_write_order`, so offsets differ even though the delta
  structure matches; search is single-threaded (git splits large lists over `pack.threads`); the "already one pack" shortcut
  skips repacking; the hash-bucket culling of `diff-delta.c` (`HASH_LIMIT`) is approximated and the rolling-hash tables are
  generated, not copied.
- **diff on conflicts**: `--ours/--theirs/--base/-1/-2/-3`, `--stat` of conflicted paths (refused), combined diff of
  binary files, symlinks or mode conflicts (refused).
- **stash**: `branch/create/store/save`, `show -u`; `-k` with a pathspec follows the scenarios only.
- **clean**: `-i` does not read a pager/`color.*`; `-e` patterns are matched like `.gitignore` lines relative to the root.
- **rev-list**: only `--count --max-count -n --all --reverse --objects --first-parent` and `^X`, `A..B`, `A...B`; no
  `--parents --boundary --left-right --no-merges --since --author --pretty --abbrev-commit --topo-order --date-order`.
- **show-ref** `--exists --abbrev`; **for-each-ref** `%(if)`, `%(contents…)`, dates, `--merged --contains --points-at`,
  `%(upstream)`, `%(authorname)`…; **merge-base** more than two commits, `--octopus --independent --fork-point`;
  **describe** `--dirty --all --match --exclude --contains --first-parent --candidates`; **shortlog** stdin mode, `-w`,
  `--group`, `--format`, mailmap.
- **rebase**: `-i`, `--root`, `--rebase-merges`, `--autostash`, `--exec`, `--keep-base`, `--no-ff/--force-rebase`,
  `--quit`, `--edit-todo`, `--reapply-cherry-picks`, `--empty=`, rerere, hooks, `rebase.*` config. The `rebase-merge/` files
  use git's names but were only checked by rivet's own `--continue`; git continuing a rivet-stopped rebase (or the
  reverse) is untested. Patch equivalence uses rivet's own patch id, not git's number.
- **grep**: `-P`, `-o`, `--and/--or/--not`, `--heading`, `-z`, `--threads`, `-p/-W`, `--untracked`, `--no-index`,
  `--recurse-submodules`, back-references. The matcher is a backtracking one, so a pathological pattern is slow.
- **blame**: `-C/-M`, `--porcelain`, `--incremental`, `-w`, `--contents`, `--since`, `-S`, regex `-L`. Blame of a
  modified work tree file (the "Not Committed Yet" lines) is implemented but has no scenario (its date is the clock).
- **apply**: `--stat --numstat --summary --3way --reject`, whitespace options, binary patches, symlink patches, fuzz.
- **format-patch**: cover letters, threading, attachments, `-k`, pathspec limiting, non-ASCII headers (RFC 2047); the
  default signature carries rivet's version (git's carries git's), so byte comparisons use `--no-signature`.
- **am**: `--3way`, `--signoff`, `-k`, `--directory`, `--whitespace`, scissors, `--show-current-patch`, `--quit`, RFC 2047
  headers, non-mbox input; `am --abort` after HEAD moved warns like git but was not scenario-tested.

### Bugs found while testing and **not** fixed (outside this block's list)
- `merge -q` omits `Auto-merging <path>` which git prints even with `-q`.
- `status --short` (and the `-s` format) from a subdirectory prints root-relative paths; git prints cwd-relative ones.
- `add <path>` of a deleted, unmerged path fails with `pathspec … did not match`; git stages the removal.
- Revision parsing has no `<ref>@{N}` reflog selector (`reset --hard topic@{1}`, `switch -c x topic@{3}` fail).
- `status` prints `HEAD detached at <short id>`; git prints the name the detach used (`HEAD detached at refs/heads/topic`).
- `tag -a <name> <annotated tag>` lacks git's nested-tag hint; `log --graph`, `checkout -m`, `switch --merge` remain
  unimplemented (already listed above).
- A stale `TREE` extension survives index edits made through `Index::insert/remove_path`; `apply` drops it explicitly before
  writing, other callers were not audited.

### Gates (2026-10-22, after the last commit of the block)
- `cargo fmt --check`: clean (exit 0)
- `cargo clippy --workspace --all-targets -- -D warnings`: clean (exit 0)
- `cargo test --workspace`: 200 passed, 0 failed, 0 ignored (includes `difftest` running every scenario, the new
  `pack::deltify` and `cmd::regex` unit tests)
- `bash tools/difftest/scenarios.sh`: 106 PASS, `failures: 0` (exit 0) — 74 before this block, 32 added

## Handoff to the unattended block (2026-10-22 12:30)
- Base commit `a71c2be`: `f869b10` (docs.rs surface) plus a test-fixture flake fix. Tree clean.
- Fixed before the launch: the `sideband_git_differential` fixtures were named with pid + nanoseconds,
  and two of its tests run in parallel threads, so they shared one directory and raced —
  `cannot copy '.../hooks/pre-merge-commit.sample': File exists` and `chmod on .../config.lock failed`.
  `cargo test --workspace` therefore failed at random on an untouched tree. Names now carry a
  per-process sequence number; the binary passes 3/3.
- Gate re-verified by the orchestrator on `a71c2be`: `cargo fmt --check` clean, clippy
  `--workspace --all-targets -- -D warnings` clean (0 warnings in the log), every `test result: ok`,
  `scenarios.sh` 106 PASS / `failures: 0`.
- Brief for the block: `.claude/HANDOFF.md`. Live checklist: `.claude/PRIORITIES.md`.

## Autonomous block (2026-10-22)

### Phase 1.1 — `merge` `Auto-merging <path>` (landed, scenario `s_merge_auto_merging`)
Real divergence was wider than `-q`: rivet never printed `Auto-merging` for a *clean* content merge (with or
without `-q`); the loop in `print_merge_report` filtered on `kind.is_some()`, which is `None` for clean paths.
Now `merge_commit` derives the list from the base/ours/theirs blobs (present in all three, all differ) and prints
it; `-q` leaves it alone, as git does. Base output before the fix: git printed `Auto-merging f`, rivet printed nothing
(scenario FAIL on the base commit, PASS after).
Gate: fmt clean, clippy clean, `cargo test --workspace` 206 passed / 0 failed / 0 ignored, `scenarios.sh` 111 PASS / `failures: 0`.
Known gap: `cherry-pick`/`revert`/`rebase`/`stash` still pass an empty list (their clean content merges do not print
`Auto-merging`); ordering of clean vs conflicted `Auto-merging` lines is clean-first, git interleaves by path.

### Phase 1.2 — `status` paths relative to the cwd (landed, scenario `s_status_subdir`)
Long and `--short` formats now print cwd-relative paths (`../other/b`, `a`, `deep/d -> deep/e`, untracked dirs keep
their trailing slash); `--porcelain` stays root-relative. Checked `-uall`, `-uno`, `-b`, renames, and runs from
`sub/`, `sub/deep/` and `other/`. The scenario failed on the base commit (rivet printed `sub/a` etc.) and passes now.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 112 PASS / `failures: 0`.
Found, not fixed (separate divergence, not in the brief): `status <pathspec>` (e.g. `status -s other`) filters only the
untracked list; tracked staged/unstaged changes outside the pathspec are still printed. Git limits all of them.

### Phase 1.3 — `add` of a deleted, unmerged path (landed, scenario `s_add_deleted_unmerged`)
`add_paths` only counted stage-0 entries when deciding whether a path missing on disk was "known", so a path that
exists only as conflict stages 1–3 was rejected with `pathspec … did not match`. The check now counts any stage; the
existing deletion pass already drops every stage (`remove_path`), matching git (`D  f`, other conflicts untouched).
Scenario covers UU/DU/UD files, a nonexistent path, and finishing the merge with `commit`; it failed on the base commit
(`fatal: pathspec 'f' did not match any files`, exit 128 vs git's 0) and passes now.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 113 PASS / `failures: 0`.

### Phase 1.4 — `<ref>@{N}`, `@{N}`, `@{-N}` (landed, scenario `s_reflog_selector`)
Implemented in `rivet-core/src/revparse.rs` (`resolve_reflog`, `previous_checkout`, `dwim_ref`; no public API change).
Rules checked against git 2.56: `<ref>@{N}` reads that ref's own reflog (DWIM'd name, `HEAD` literal); bare `@{N}`
uses the *current branch's* reflog (HEAD when detached); `@{-N}` is the branch N checkouts ago and may prefix `@{N}`
(`@{-1}@{1}`); N may have leading zeros; suffixes compose (`topic@{1}~1`, `^{tree}`). Past the end: fatal
`log for '<name as typed>' only has M entries` (exit 128; `@{99}` names the current branch); one past the oldest entry
returns that entry's *old* id when it is not null (git's quirk, exercised by editing the log in the scenario).
A ref without a reflog, `@{x}`, `@{}`, `x@{1}@{1}` stay "ambiguous argument", as in git.
CLI: `resolve_rev`, `rev-parse`, `reset`, `branch`, `switch -c`/`checkout -b` let that one fatal through instead of
their generic wording (`rev_error` in `cmd/mod.rs`). Base: every `@{N}` spec failed as "ambiguous argument".
Not implemented (still invalid revision, as before): date selectors (`main@{yesterday}`), `@{upstream}`/`@{u}`/`@{push}`.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 114 PASS / `failures: 0`.

### Phase 1.5 — `HEAD detached at <name>` (landed, scenario `s_status_detached_name`)
`cmd::detached_description` follows git's `wt_status_get_detached_from`: newest `checkout: moving from X to <name>` entry
of the HEAD reflog; `<name>` expanded like a ref name; kept as `refs/heads/…` for branches, `refs/tags/` and
`refs/remotes/` stripped, pseudo-refs (`HEAD`) and non-ref specs (`main~1`, shas) fall back to the short id; the word is
`at` only while HEAD still equals the id that checkout produced, otherwise `from`. Used by `status` (long format) and
`branch` (`* (HEAD detached at …)`). Scenario covers branch, `refs/heads/`, `heads/`, tag, annotated tag, remote ref,
`main~1`, `topic^0`, `HEAD`, short and full sha, `@{-1}`, then commits/reset after detaching (`from`).
Two further real divergences found by the scenario and fixed with it:
- **Odd-length abbreviated ids never resolved** (`rev-parse 2461a6d` → unknown revision, base commit too; only even
  lengths worked, git's default abbrev is 7). Fixed in `revparse::resolve_base` by matching the odd last nibble against
  each id's hex text; own scenario `s_abbrev_odd_lengths` (lengths 4–12, 39, 40, uppercase, no-match).
- `switch --detach <x>` while already detached at that commit wrote a reflog entry; git writes none.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 116 PASS / `failures: 0`.

### Phase 1.6 — nested-tag hint on `tag -a` (landed, scenario `s_tag_nested_hint`)
**The brief's expected text is wrong**: git 2.56 does not print `warning: tag 'x' is actually an annotated tag`. It
prints a five-line `hint:` block on stderr, only when an *annotated* tag (`-a`/`-m`/`-F`) is created on an object that
is itself a tag (`hint: You have created a nested tag. …`, `hint: \tgit tag -f <name> <object-as-typed>^{}`,
`hint: Disable this message with "git config set advice.nestedTag false"`). Lightweight tags and commit targets print
nothing; `advice.nestedTag=false` silences it; an explicit `advice.nestedTag=true` drops the "Disable" line. Git won,
so that is what `tag.rs` does. The scenario failed on the base commit (git printed the hint, rivet nothing).
Found, not fixed: rivet has no global `git -c key=value` option (`rivet -c … tag` → clap error); other `advice.*`
keys are still not honoured by the existing hints.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 117 PASS / `failures: 0`.

### Phase 1.7 — stale `TREE` index extension (landed, scenario `s_index_cache_tree`)
Worse than the brief said: this was a silent-corruption bug, not only an unaudited code path. After real git wrote a
cache-tree (`git commit`, `git write-tree`), a rivet `add`/`rm`/`mv`/`restore --staged`/`reset <path>`/`apply --cached`
kept the old `TREE` extension, and then real `git write-tree` returned the **old tree id** and `git diff --cached` showed
nothing, so a following `git commit` would have recorded the pre-edit tree (base: `git write-tree` → `5bd3e5b…` for every
step, git-twin got a distinct id per step).
Rule implemented in the index layer (`Index::insert`, `remove`, `IndexExt::remove_path`; `resolve` goes through them):
any change to the entry set drops the cached-tree extension; an `insert` that only refreshes stat data (same oid + mode)
keeps it. Deliberate deviation: git invalidates only the touched path's directories and keeps valid sibling nodes; we
drop the whole extension (valid, just recomputed by readers), so index *bytes* differ from git's after an edit although
`ls-files`, `write-tree` and `fsck` agree. The `apply` workaround in `cmd/patch.rs` is deleted.
`entries_mut()` has no callers (checked) and still documents the caller's responsibility.
Harness: new `vg` helper in `tools/difftest/lib.sh` — runs real git and records its output/exit code in the transcript, so
what git reads out of a rivet-edited repository is compared (the old `g` helper discards output).
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 118 PASS / `failures: 0`.

### Phase 1.8 — `switch --merge` / `checkout -m` (landed, scenarios `s_switch_merge`, `s_switch_merge_states`, `s_switch_merge_detached`, `s_checkout_merge`, `s_switch_merge_refused`)
Git 2.56 does not do the old two-tree merge for this: it is an **autostash**. Verified behaviour, now implemented:
- the plain switch is tried first; only a refusal for tracked local changes (`DirtyWorktree`) starts the autostash, so a
  clean tree, a switch that does not overlap the changes, and an untracked-file collision (same error as without `-m`)
  never stash anything;
- stash = `git stash create`+`store` shape: commit `On <branch|(no branch)>: autostash while switching to '<target>'`
  (target as typed), index commit as `stash push`, reflog message *without* the `On …:` prefix, then reset to HEAD with
  **no** reflog line and no ORIG_HEAD; then the normal switch (`checkout: moving from X to Y`);
- apply = merge of the stash on top of the new HEAD, markers `<<<<<<< <target>` … `>>>>>>> local`. Clean: stderr
  `Applied autostash.`, entry dropped silently, everything unstaged. Conflicts: stderr 5-line "Your local changes are
  stashed…" block, entry kept, **exit 0**, stages 1/2/3 in the index, cleanly merged paths staged, a blank line before
  `Switched to …`/`HEAD is now at …`, even with `-q`;
- stdout after an autostash: `The following paths have local changes:` then `M/A/D<TAB>path` lines (unmerged paths print `M`).
Code: `stash::autostash_save/autostash_apply` (+ `remove_entry`, the silent half of `drop_at`), `switch_to(..., merge)`.
Also fixed because the scenarios exposed them (plain commands, no `-m`): `switch <tag|rev-expression>` now prints git's
`fatal: a branch is expected, got …` plus the `hint: If you want to detach HEAD…` line (rivet crashed on `topic~0` with an
invalid-ref-name error and never printed the hint); a detached HEAD "moving" to where it already is (`reset --hard`)
writes no reflog entry (`ops::move_head`).
Not implemented (refused with a message, never silently different): `--conflict` (needs a base label in `MergeOptions`;
`merge.conflictStyle=diff3` is also ignored by rivet's `merge`, separate divergence: git prints `||||||| <short id>`),
`switch -m -c/-C/--orphan`, `checkout -m` with paths (`checkout -m -- f` re-creating conflicts), `-m` combined with
`--force`. The `run_switch` `///` text still describes the pre-2.56 model ("merge in progress"); it was left alone because
doc comments are frozen.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 123 PASS / `failures: 0`.
`log --graph` and `checkout -m` with paths were not attempted (brief: leave `log --graph` for another block).

### Phase 2.1 — `rev-list` flags (landed, scenarios `s_rev_list_filters`, `s_rev_list_abbrev`, `s_rev_list_order`, `s_rev_list_boundary`)
New flags: `--parents`, `--no-merges`, `--merges`, `--min-parents/--max-parents`, `--since/--after`, `--until/--before`,
`--author`, `--committer`, `--grep` (+ `-i`, `-E`, `-F`, `--all-match`, `--invert-grep`), `--abbrev-commit`,
`--no-abbrev-commit`, `--abbrev[=N]`, `--topo-order`, `--date-order`, `--author-date-order`, `--boundary`.
Ordering is now a port of git's own algorithm (`cmd/gitwalk.rs`), not a sort of the reachable set: date-sorted work list with
`commit_list_insert_by_date` tie rules, `limit_list` with its 5-commit clock-skew slop (ranges, topo orders), plain walk
that stops *behind* a too-old commit for `--since`, `sort_in_topological_order` (LIFO / committer-date / author-date queue),
`--all` = refs in name order then HEAD. `log` still uses the old core walk (not touched).
Verified-against-git details worth keeping: `--parents` prints *all* parents even with `--first-parent`, and never abbreviates
them; `--abbrev=N` without `--abbrev-commit` prints full ids; auto abbreviation length follows `find_unique_abbrev`
(`ceil(bits(objects)/2)`, min 7); `--author`/`--committer` match `Name <email>` (not the date); `--boundary` candidates are the
not-yet-shown parents of each shown commit (any parent, not only excluded ones), pushed on the front of a list that is then
topologically sorted with the active order (graph order by default) — and a commit never parsed (second parent under
`--first-parent`) sorts as dated 0 with no parents, but keeps its author date.
Besides the scenarios (fixed history with a skewed clock, merges, 5 authors, distinct author/committer dates; ~170 commands
compared in full) a throwaway fuzzer (130 random merge-heavy repos × ~90 flag combinations, git vs rivet, outputs
identical) was used to find and close the ordering corner cases above; it is not in the tree.
Also fixed in core because `--since` exposed it: `parse_approxidate`/`parse_git_date` mis-parsed `YYYY-MM-DD HH:MM:SS +ZZZZ`
(a space before the offset; a date with dashes was taken for an epoch) and a bare `YYYY-MM-DD` was midnight where git uses
the current time of day (checked: `--since=2023-11-14` at 18:13 excludes an 18:00 commit). Shared with `log --since`.
Not implemented: `--skip`, `--max-age/--min-age`, `--left-right`, `--cherry-pick`, `--ancestry-path`, `--children`,
`--stdin`, pathspec limiting, `--format`/`--header`, relative dates (`2.days.ago`) — unknown flags are clap errors, relative dates
an "invalid date format" error.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 127 PASS / `failures: 0`.

### Phase 2.2 — `for-each-ref` atoms, sorting, filters (landed, scenarios `s_for_each_ref_atoms`, `_dates`, `_contents`, `_messages`, `_upstream`, `_if`, `_sort`, `_filters`; old `s_for_each_ref` unchanged and green)
New module `cmd/foreachref.rs` replaces the five-atom evaluator. Format language: `%%`, `%NN` (hex byte; a lone `%` or `%x09` is literal),
`%(if)`/`%(if:equals=)`/`%(if:notequals=)` … `%(then)` … `%(else)` … `%(end)` (nested), `%(align:W[,left|middle|right])`,
`%(color:…)` (only with `--color[=always|auto|never]`; `color.ui` does not enable it, as in git), git's error texts for
mismatched `then/else/end`, `malformed format string` (usage text, exit 129).
Atoms: `refname[:short|lstrip=N|rstrip=N|strip=N]`, `objecttype`, `objectsize`, `objectname[:short[=N]]`, `tree`, `parent`,
`numparent`, `object`, `type`, `tag`, `author|committer|tagger|creator`, `…name`, `…email[:trim|:localpart]`,
`…date[:default|iso|iso-strict|short|raw|unix|rfc|rfc2822|relative|format:<strftime>]`, `subject`, `body`,
`contents[:subject|:body|:signature|:lines=N]`, `upstream[:short|:track|:trackshort|:remotename|:remoteref][,nobracket]`,
`push[:short|:remotename]` (push.default simple/current/nothing), `symref`, `HEAD`; `*atom` dereferences annotated tags down to the
first non-tag. Sorting: any atom, `-` prefix, last `--sort` is primary (the old code made the first primary), refname ascending as the
final tie-break, numeric for dates/sizes/numparent, `--ignore-case`, `--count` (0 = all). Filters: `--merged`, `--no-merged`,
`--contains`, `--no-contains` (optional value, default HEAD, and — as in git — they swallow the next argument even if it starts with
`-`), `--points-at` (any level of a tag chain), `--exclude`, `--omit-empty`.
Verified-against-git details: `refname:dirname/basename` and `creatorname/creatoremail` do not exist in git 2.56 (unknown field /
unrecognized argument); `contents:lines=N` indents continuation lines by four spaces; `subject` joins the first paragraph with
spaces and drops its trailing newline; `contents` skips leading blank lines; `push` is empty unless the upstream branch has the
branch's own name; `%(color)` attributes come out in git's fixed order (bold, dim, ul, blink, reverse, italic, strike); bad
`--merged`/`--contains` values print `malformed object name X` (fatal/128 vs error/129), `--points-at` prints it quoted, exit 129.
Not implemented: `--shell/--perl/--python/--tcl`, `--stdin`, `--start-after`, `--include-root-refs`, `version:`/`v:` sort,
`%(trailers)`, `%(contents:trailers)`, `%(rest)`, `%(worktreepath)`, `%(deltabase)`, `%(objectsize:disk)`, `%(ahead-behind:…)`,
`%(authordate:local|human)`, `%(refname:short)` strictness options, commit `gpgsig` in `contents:signature` (tag signatures only).
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 135 PASS / `failures: 0`.

### Phase 2.3 — `blame -M`, `-C`, `--porcelain` (landed, scenarios `s_blame_move_copy`, `s_blame_porcelain`; `s_blame`, `s_blame_edits` unchanged and green)
`cmd/blame.rs` is now a port of git's `blame.c` rather than a per-line pass: the final file is a list of entries (`lno`, `num`,
suspect, `s_lno`); a commit-date work queue; `pass_blame` / `pass_blame_to_parent` (unchanged diff runs move to the parent, the
rest stays as separate entries), `filter_small`, `blame_entry_score` (1 + alphanumerics of the entry's final lines, entries
scoring ≤ threshold are ignored: `-M` 20, `-C` 40, `-M<n>`/`-C<n>` adjust), `find_copy_in_blob` + `copy_split_if_better`
(best-scoring unchanged run of a line diff, ties go to the later candidate), `find_move_in_parent`, `find_copy_in_parent`
(candidate files = the ones the commit changed or removed; `-C -C`: every file of the parent when the file is new in the commit;
`-C -C -C`: always), `coalesce` for group sizes. `-C` implies `-M`; `-M<n>`/`-C<n>` are rewritten to long options in `main.rs`
because clap cannot take an optional attached value without swallowing the next argument.
Porcelain: `--porcelain`/`-p`, `--line-porcelain` with `previous <sha> <path>` (first parent version the file was compared with,
renames included), `boundary` for root commits, `filename` once per commit (and for every group when a commit is blamed under
more than one path), group sizes on the first line of each group, "Not Committed Yet" details for uncommitted lines (time and
zone are the current ones, as in git).
Verification beyond the scenarios: a throwaway fuzzer (110 random histories with branches, merges, renames and edits / inserts /
deletes / moves / copies / duplicated runs of short and long lines; for every file at HEAD and three earlier revisions, 12 flag
combinations including `-M`, `-C`, `-C -C`, `-C -C -C`, `-M5`, `-C10`, `-C5 -C5`, `--porcelain`, `--line-porcelain`, `-f`) found
no difference from git; `-M`/`-C` changed the output in most of those repositories, so the heuristics were exercised.
Not implemented: `-M`/`-C` while the file has uncommitted changes (refused with a message), `--incremental`, `-w`, `--since`, `-S`,
`--contents`, `--reverse`, `--root`, `-n`/`--show-number`, `-t`, `--date`, `--abbrev`, regex `-L`.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 137 PASS / `failures: 0`.

## Block 3 (autonomous run, base `3f10b26`)

### Phase A.1 — `apply --stat/--numstat/--summary/-z/--apply` (landed, scenarios `s_apply_stat`, `s_apply_stat_renames`)
Output is `apply.c`'s own layout, not `diff --stat`'s: name column = longest quoted name capped at 50 (longer names become
`...` + the tail from the first `/` at or after `len+3-50`); graph scaled with `max = 70 - name_width` when
`name_width + max_change > 70`, else by `max_change`, rounded (`(n*max + max_change/2) / max_change`); the count is `|%5d ` (a
trailing space even for 0); binary files are ` |  Bin`. Blocks come out in the order stat, numstat, summary. Stat and numstat names
are C-quoted; summary names are not. Nothing is applied unless `--apply`; with `--check` or `--apply` the patch is checked first and
a patch that does not fit prints no statistics (exit 1).
Verified against `/usr/bin/git` where the brief differed: `--stat` and `--numstat` list only the **new** name of a rename (never
`old => new` in the stat, never `old\0new` with `-z`: `-z` is `added\tdeleted\tnew\0`); `--summary` renames use `dir/{a => b}` with
only the common *leading* directories factored out; the `mode change` line of a rename carries no name; `-R` lists the patches
in reverse order but sizes the name column from the unreversed names.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 139 scenarios / `failures: 0`.
