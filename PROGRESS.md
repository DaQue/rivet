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

### Phase A.3/A.4 — `apply --reject` and binary patches (landed, scenarios `s_apply_reject`, `s_apply_binary`)
`--reject` follows git, not `patch(1)`: it turns on the narration (`Checking patch X...` per patch, then `Applied patch X cleanly.`
or `Applying patch X with N reject(s)...` + `Hunk #i applied cleanly.` / `Rejected hunk #i.`), a hunk that does not fit prints
`error: while searching for:` + its pre-image + `error: patch failed: f:LINE` (there is no "N out of M hunks failed" in git), the
other hunks and the other files still go in, `<path>.rej` holds `diff a/<path> b/<path>\t(rejected hunks)` and the rejected hunks
exactly as the patch has them (`Hunk::raw`); an existing `.rej` is overwritten, and so is the one of an earlier section for the same
file. Exit 1 when any hunk was rejected or any file errored; then the index is left alone even with `--index`/`--cached`
(git writes it only after a fully clean run). `--reject --check` reports and exits 0 when only hunks fail. `-R` goes through the
patches last to first, names the line of the reversed hunk, and `Checking patch new => old...` for renames.
Binary: `Binary files … differ` without full ids → `cannot apply binary patch to 'f' without full index line` + `f: patch does not
apply`; with a full `index` line (`diff --full-index`) git applies it from the object database, and so does rivet now: wrong
pre-image → `the patch applies to 'f' (<oid>), which does not match the current contents.`, post-image blob not in the odb →
`missing binary patch data for 'f'` / `binary patch does not apply to 'f'` / `f: patch does not apply`. The existence checks
(`No such file or directory`, `does not match index`) come before the binary refusal, as in git.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 141 PASS / `failures: 0`.

Known gaps (apply): `GIT binary patch` (diff `--binary`) data is **not** decoded — git applies it, rivet prints
`error: binary patches are not supported` (not git's text) and exits 1 without touching anything. Command:
`git diff --cached --binary >p; rivet apply p` → git exit 0, rivet exit 1. Decoding needs zlib in rivet-cli (no crate dependency
change allowed in this block) or a public inflate in rivet-core (API change).

### Phase A.2 — `apply --3way` (landed, scenario `s_apply_three_way`)
`--3way` implies `--index` (unless `--cached`) and is tried **first** for every patch, as in git's `apply_data`: the pre-image is the
blob named by the (abbreviated) `index <old>` id, the post-image is the patch applied to that blob, and the post-image is merged into the
current content with `merge_content` (labels `ours`/`theirs`, base text not printed). Messages: `Applied patch to 'f' cleanly.` /
`... with conflicts.`; when the blob is missing or the `index` line absent `error: repository lacks the necessary blob to perform
3-way merge.` + `Falling back to direct application...` (then an ordinary direct apply, which may still fail); a pre-image the hunks do
not fit prints `error: patch failed: f:N` before the fallback; deletions, and creations of a file that does not exist, fall back silently;
creating a file that already exists is not an error with `--3way` — `Performing three-way merge...` with an empty base. A conflicted path
leaves the merged text with markers in the work tree, stages 1/2/3 in the index (stage 1 absent for a creation), `U <path>` on **stderr**,
exit 1; the other patches still go in. `--check` reports the same lines, writes nothing and exits 0 on conflicts. `--3way --reject` is
`error: options '--reject' and '--3way' cannot be used together`, exit 128. The empty blob and the post-image/"ours" blobs are written to
the odb exactly as git does (the `fsck` dangling list of the scenario compares them).
Not covered: `--ours`/`--theirs`/`--union`, `-v`, `core.autocrlf` effects on the merge.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 142 PASS / `failures: 0`.

### Phase B — `gc` cruft packs, write side (landed, scenarios `s_gc_cruft`, `s_gc_cruft_expire`; `s_gc_after_cruft` tightened)
Verified against git 2.56 on identical fixtures (absolute mtimes set with `touch -d`, absolute `--prune=` dates): `gc --cruft` /
`--no-cruft`, `gc.cruftPacks` (**default true** — git 2.56's default, so a plain `rivet gc` now writes a cruft pack; the older
`s_gc*` scenarios pin `gc.cruftPacks=false`), `--prune=<date|now|never>`, `--no-prune`, `gc.pruneExpire`.
* Reachable objects → the main pack as before; every other object (loose, or in a pack that is being replaced, old cruft packs
  included) is a candidate. Age: loose file mtime, the pack's mtime, or the age in an old cruft pack's `.mtimes`; the newest wins.
* Expired = age **<=** the cutoff (an object exactly at the cutoff is gone). Everything an unexpired candidate reaches is kept even if
  expired, and recorded with the cutoff as its age (`reached_from`). `--prune=now` therefore writes no cruft pack; no expiry keeps all.
* Cruft pack layout, as `git pack-objects --cruft` writes it: the unexpired objects first — with an expiry loose objects (by id)
  then each old pack's objects in pack order, newest pack first (equally old: descending name); without an expiry the packs first,
  then the loose ones — then the objects kept for connectivity in walk order (commits newest first, then each commit's trees and
  blobs, trees named directly before those). Deltas are searched like the main pack (name hash 0 for the first group); a delta's base is
  written right before it. `pack-<id>.mtimes` = `MTME`, 1, hash id, one big-endian u32 per object in `.idx` order, pack checksum,
  file checksum. `pack-<id>.rev` is now written for every pack gc writes (git does; `s_gc_cruft*` compare the file list).
* Old cruft packs and the loose copies of packed objects are removed only after the new packs verify; a cruft pack whose bytes did not
  change keeps its name.
* Non-cruft pruning now keeps the old objects a young unreachable object needs, as `git prune` / `repack -A --unpack-unreachable`
  do (found by the `--no-cruft --prune=<date>` rows of the fixture matrix: git kept 3 loose objects, rivet 1).
Differences from the brief, git being the oracle: `git gc` has **no** `--cruft-expiration` (that is `git repack`; `gc` uses `--prune` /
`gc.pruneExpire` for it, so `s_gc_cruft_expire` drives expiry through those), no `--cruft-expiration` was added to `rivet gc`.
Compared (via real git reading rivet's repository): `verify-pack -v` shape in pack order for the cruft pack (type, size, delta base —
not compressed sizes or offsets, which depend on the zlib build: git stores a 90-byte tree in 92 bytes, rivet in 103), the age table
of the `.mtimes`, `cat-file --batch-all-objects --batch-check`, loose leftovers, pack file extensions, `fsck`, `count-objects -v`.
Known gaps (gc): the main pack's *write order* is commits, trees, blobs while git writes commits, blobs, trees for an untagged
repository (its `to_pack` order, trees after their entries) — object set and delta shapes match, so `s_gc*` compare it sorted;
`rivet fsck` lists dangling objects in a different order than git once a cruft pack exists (the scenarios sort it with `vs`);
`--max-cruft-size`, `--expire-to`, `--keep-largest-pack`, `gc.bigPackThreshold` are not implemented (clap refuses them); `--no-cruft`
writes unpacked objects back loose with *the time of the write* (as git does) — the old attempt to restore the pack's age silently
failed on the read-only loose file and was left alone. Found on the way: `rivet stash drop -q` still prints `Dropped refs/stash@{0}
(<id>)` (git is silent), not touched.
Gate: fmt clean, clippy clean, doc clean, 206 tests passed / 0 failed, `scenarios.sh` 144 PASS / `failures: 0`.

### Phase C — completeness proof (landed; tests in `crates/rivet-cli/tests/robustness.rs`)
* Crate-level `#![allow(dead_code)]` / `#![allow(unused_imports)]` and the "signature-only scaffold" module docs were already gone at
  the base commit (`git grep` finds neither; the only `allow`s left are nine `clippy::too_many_arguments`/`len_without_is_empty` with
  reasons). `cargo build`/`clippy -D warnings` report no dead code, so there was nothing to delete.
* `unwrap()`/`expect()` audit: 46 sites outside test modules. All but the ones below are guarded by a check a few lines earlier
  (`split_once(..).expect("checked")`, fixed-size `try_into().unwrap()` after a length check, `required[..]` flags in the commit
  parser, ...) and were left alone. To look for what the audit by reading cannot see, I fuzzed the release of this block with
  throw-away drivers (kept outside the repo): mutated patches (15 000 runs over `apply --stat/--check/--3way/--reject/-R/--index/
  --cached/-z` and 7 500 writing runs), mutated repository files (index, loose objects, packs, `.idx`, `.rev`, config, packed-refs,
  reflogs, refs; 6 300 runs × 6–8 commands), mutated commit/tree/tag objects (2 500 runs × 6 commands), random
  revision/pathspec/URL/config arguments to ~100 commands (14 000 runs), random `.gitignore`/`.gitattributes`/pathspecs
  (600 runs × 5 commands), mutated mailboxes for `am`/`mailinfo` (400 runs × 2 commands), looking for exit 101 / `panicked`. Three panics, all fixed, each with a test:
  1. `apply --3way` with a non-ASCII `index` id sliced the string inside a character (`patch.rs`, my own code from this block):
     `apply_three_way_with_a_garbled_index_id_falls_back_to_a_direct_apply_instead_of_panicking`.
  2. `IndexEntry::parse` read the flags word before checking that it was there (`index out of bounds` on an index cut inside an
     entry): `a_truncated_index_entry_is_an_error_not_a_panic_and_the_index_file_is_untouched` (fails on the old code, checked).
  3. `parse_rev_spec` (`log '...x'`, `log ...<anything without a dot>`) sliced past the end; the split is rewritten to find `...` then
     `..` in the raw bytes and an empty side is `None` as its docs say. `rivet log` called it and threw the result away — that dead
     call is removed: `log_with_a_leading_three_dot_spec_naming_nothing_fails_cleanly_and_moves_no_ref` (also checks `...side` against git).
* One state-asserting test per behaviour added in phases A and B, read back with real git: `apply_stat_summary_and_numstat_leave_the_
  tree_and_index_alone_until_apply_is_given`, `apply_reject_writes_the_failed_hunk_applies_the_rest_and_leaves_the_index_alone`,
  `apply_three_way_conflict_leaves_the_three_stages_and_markers`, `apply_refuses_a_binary_patch_without_full_ids_and_applies_none_of_
  the_patch`, `gc_without_cruft_keeps_the_old_objects_a_young_unreachable_commit_still_needs`, `gc_cruft_packs_the_young_commit_with_
  its_old_tree_and_blob_and_records_their_ages`.
* Seen while fuzzing, not panics, not touched: `rivet remote -v` lists a url-less remote as `(fetch)`/`(push)` lines (git prints the
  bare name), `rivet branch -v` dies on a branch pointing at a missing object (git warns and skips it), `rivet log ...x` says
  `fatal: not a valid object name: 'x'` (git: `ambiguous argument`).
Gate: fmt clean, clippy clean, doc clean, 215 tests passed / 0 failed / 0 ignored, `scenarios.sh` 144 PASS / `failures: 0`.

## Summary of block 3
Landed: `apply` `--stat/--numstat/--summary/-z/--apply`, `--reject`, `--3way`, binary patches with full ids (A); `gc` cruft packs
with `.mtimes`/`.rev`, `gc.cruftPacks` default true, retention of what young unreachable objects need (B); panic fixes + 9 tests (C).
Left undone on purpose: `GIT binary patch` data (see Known gaps under A), `gc --max-cruft-size/--expire-to/--keep-largest-pack`,
`log --graph`, `checkout -m` with paths, `merge.conflictStyle=diff3` markers in `merge`.

## Block 4 (autonomous run, base `91bf98e`)

### Phase 0 — `s_gc_cruft_expire` nondeterminism (fixed, scenario `s_gc_cruft_expire`)
Base gate re-derived: fmt, clippy, doc clean; `cargo test --workspace` all `ok`, 0 failed; `scenarios.sh` 144 PASS / `failures: 0`.
Looped `s_gc_cruft_expire` 10x on the base commit: 8 PASS, 2 FAIL (iterations 5 and 8). Both failures had the identical diff:
the `.mtimes` block of the `gc --cruft` step differed by one second on all 11 ids (git `1792742524`, rivet `1792742525`).
Cause: the preceding `gc.cruftPacks=false` gc loosens the unreachable objects with mtime = wall clock, and git and rivet
run seconds apart, so the cruft pack records whichever second each side ran in. Not a `gc` bug and not an ordering issue
(hardcoded October dates and `fsck` order were not involved).
Fix: the scenario pins the loose files' age (`age_unreachable "$NEW"`) before `gc --cruft`; no assertion was weakened or sorted.
After: 30/30 PASS, plus 8 more runs of `s_gc_cruft` + `s_gc_after_cruft`, all `failures: 0`. Full suite: 144 PASS / `failures: 0`.

### Phase 1.1 — `stash drop -q` / `stash pop -q` silent (landed, scenario `s_stash_stack` extended)
`drop_at` printed `Dropped ...` unconditionally. On the base the extended scenario failed: rivet printed
`Dropped refs/stash@{0} (...)` after `stash drop -q`, `Dropped stash@{0} (...)` after `stash drop -q stash@{0}`, and
`Dropped refs/stash@{0} (...)` after `stash pop -q`; real git prints nothing (verified with `/usr/bin/git`). `-q` now
suppresses it; the scenario also compares `stash list` and `status --short` after each. Gate: fmt, clippy clean;
215 tests passed / 0 failed; `scenarios.sh` 144 PASS / `failures: 0`.

### Phase 1.2 — `remote -v` without url, `branch -v` on a missing object, `log` unknown-revision text (landed, scenarios `s_remote_fetch_push`, `s_branch_ops` extended)
Each was red on the base against `/usr/bin/git`:
- `remote -v` with `remote.nourl.fetch` only: git prints one line `nourl<TAB>` (no label); rivet printed `nourl<TAB> (fetch)` and `nourl<TAB> (push)`. With a pushurl git prints `nourl<TAB>` then `nourl<TAB>../origin.git (push)`.
- `branch -v` with `refs/heads/ghost-with-a-long-name` naming a missing object: git silently omits it (also from the column width, and with `--merged`); rivet died with `fatal: object not found: 111...`.
- `log '...x'`, `'x...'`, `'HEAD...x'`, `'x..'`, `'..x'`, `log x`: git says `ambiguous argument '<whole arg>': unknown revision or path not in the working tree.` + the `Use '--'` hint; rivet said `not a valid object name: 'x'`. `log '^x'`: git says `bad revision '^x'`.
Gate: fmt, clippy clean; 215 tests passed / 0 failed; `scenarios.sh` 144 PASS / `failures: 0`.
Not done: `branch --contains` is not accepted by rivet (`error: unexpected argument '--contains'`), found while probing; `log` of an existing non-commit object now also reports `ambiguous argument` (not compared with git).

### Phase 1.3 — `fsck` dangling/unreachable order (landed, every `fsck` call in `scenarios.sh` is now unsorted)
Previously the scenarios used `vs fsck` (sorted output) and rivet listed ids in ascending order. With the sort removed, git's order
after a cruft pack was not ascending: `s_gc_cruft_expire` on the base showed git `commit c1a14e…, commit 4e7fb7…, blob 94510b…,
tag 341663…, blob 7972c0…, blob 7d4fa7…` vs rivet in id order. Git walks its in-memory object table (slot = first 4 id bytes, little-endian,
modulo the table size; 32 slots, doubling when it would pass half full; linear probing). `git_table_order` in `fsck.rs` reproduces that
(verified against the 6 ids above and 20-object table = 64 slots). Caveat: ids that collide are placed in id order, whereas git places them
in its own object-load order, so a collision could still differ; no scenario hit one. All 144 scenarios pass 3 runs in a row with no `fsck` sort.
Gate: fmt, clippy, doc clean; 217 tests passed / 0 failed (2 new unit tests); `scenarios.sh` 144 PASS / `failures: 0`.

### Phase 1.4 — main `gc` pack write order (NOT done; known gap)
`packshape` in `scenarios.sh` still sorts its lines, so write order is not asserted. Probe: remove the `| sort` and compare each pack's objects in
`verify-pack -v` order (packs keyed by object set): `s_gc_delta`, `s_gc_cruft` and `s_gc_cruft_expire` all go red against git.
Differences seen: git writes a delta base followed by its whole delta family (`bcbff6…` then its ten `blob 7 1 bcbff6…` deltas, then the
`blob … 2 6453811d…` chain), rivet interleaves the deltas of different bases; and in `s_gc_cruft_expire` git writes commits, then the three
`blob 2` blobs, then trees, while rivet writes trees before blobs. Matching it means porting `compute_write_order`
(tag chains, commit recency, trees, then families) plus the traversal order of the object list; not attempted in this block.

### Phase 2.1 — `log --graph`, `--decorate` (landed in part; scenarios `s_log_graph_linear`, `s_log_graph_merges`, `s_log_graph_random`, `s_log_decorate`)
New `crates/rivet-cli/src/cmd/graph.rs`: the `graph.c` state machine (padding / skip / pre-commit / commit / post-merge / collapsing
lines, octopus `*-.` merges, merge layouts, `_` runs, `/` carry-over). `--graph` implies topo order; `--reverse` is refused with git's text.
Compared with git on linear history (`--oneline`, medium, `--format`, `--stat`, `-p`, `--name-status`, `-n`, `--skip`), a hand-built history with
side branches and an octopus merge, and a random merge-heavy history (`s_log_graph_random`, fixed seed 7; I also ran seeds 1–140 by hand, all identical to git).
Things the oracle taught me that are not in my memory of `graph.c`: the collapsing row's cross-over branch also records the horizontal
run; the commit row prints `/` when the previous row slid into that column; the blank separator row between entries (padding in the
commit state) hides the previous row's state from those checks; a far-left parent is reached by `_` in the post-merge row.
`--decorate[=short|full|auto|no]` / `--no-decorate` were accepted but ignored; they now decorate `--oneline`, the medium layouts and `%d`
(`refs/stash`-style refs print their full name; `refs/notes/` and `refs/prefetch/` are skipped — the notes skip is my guess, not checked).
`--decorate=bogus` dies with git's `invalid --decorate option: bogus`. Also: `KEEP=1` keeps a passing scenario's directory (`lib.sh`).
Gate: fmt, clippy, doc clean; 218 tests passed / 0 failed / 0 ignored; `scenarios.sh` 148 PASS / `failures: 0`.

Known gaps (all refuse or are untested rather than wrong, except colour):
- **Several roots**: git 2.56 draws a root commit that is not shown last one column to the right (sometimes with a ` \ ` connector row);
  I could not derive the rule from probes (`git log --graph --all` over `main: m1<-m2` and `other: r1<-r2<-r3` gives `r3, r2, " \ ", "  * r1", m2, m1`).
  `rivet log --graph` exits 128 with `log: --graph over several root commits is not implemented (<id> is a root that is not shown last)`;
  test `log_graph_refuses_a_root_that_is_not_shown_last_and_prints_nothing`.
- **Colour** (`color.ui`, `--color`): not implemented anywhere in `log` (the graph code can paint columns, but nothing turns it on and the
  `commit`/decoration colours are not produced). `git -c color.ui=always log --graph --oneline --decorate` differs from rivet.
- `log --graph` with a path limit, `--boundary`, `--left-right`, `--simplify-by-decoration` and `--graph` with `-m` merge diffs are not covered by a scenario.
- Found while probing, unrelated to the graph: `rivet rm -rq -f .` dies with `refusing to remove the whole work tree` (git removes everything);
  `rivet checkout --orphan` writes a `checkout: moving from main to other` reflog line that git does not.

### Phase 2.2 — `merge --squash` (landed, scenario `s_merge_squash`)
Base: `fatal: merge: --squash is not implemented`. Now: clean/diverged, fast-forward-able (`Updating…`, `Fast-forward`, index and work tree
updated, HEAD not moved), already up to date (`Already up to date. (nothing to squash)`), and conflicted (stages kept, `MERGE_MSG`
with the `# Conflicts:` list, exit 1) cases match git, including `SQUASH_MSG` (heading + `log`-style medium entries, newest first), which is
now part of the compared state (`lib.sh`). No `MERGE_HEAD` is written, so the commit that follows has one parent; `commit` removes
`SQUASH_MSG`/`MERGE_MSG`, `clear_merge_state` (used by `reset`) removes `SQUASH_MSG`. `--squash --no-ff` dies with git's
`options '--squash' and '--no-ff.' cannot be used together`. Side fixes found by the scenario (each red on the base): `merge <unknown>` exits 1 with
`merge: X - not something we can merge` (was 128/`fatal:`); `commit` with unmerged paths prints `U\t<path>` and git's four-line refusal, exit 128
(was `fatal: path is unmerged`).

### Phase 2.3 — `pull --rebase`, `pull --squash` (landed, scenarios `s_pull_rebase`, `s_pull_rebase_conflict`, `s_pull_squash`)
Base: `fatal: pull: --rebase, --autostash, --interactive, --squash and --gpg-sign are not implemented`.
`pull --rebase` (or `pull.rebase`/`branch.<name>.rebase` = true) refuses a dirty tree before fetching (`error: cannot pull with rebase: …`, exit 128),
fast-forwards through the merge machinery when nothing local needs replaying (reflog `pull --rebase: Fast-forward`), otherwise runs the rebase machinery
with reflog action `pull --rebase` (`pull` when it came from config); conflicts stop in `rebase-merge/` and `rebase --continue` finishes.
`pull --squash` goes through `merge --squash`. Also: a divergent `pull` with no `--rebase/--no-rebase/--ff-only/--no-ff` and no `pull.rebase`/`pull.ff` is now refused
with git's `Need to specify how to reconcile divergent branches.` hint block (exit 128; base merged silently); `--no-edit` is accepted.
Gate: fmt, clippy, doc clean; 218 tests passed / 0 failed / 0 ignored; `scenarios.sh` 152 PASS / `failures: 0`.
Known gaps: `pull --rebase` uses the fetched tip as both `--onto` and upstream (git also computes a `--fork-point` from the remote-tracking reflog, which only
matters when the upstream was rewritten); `--rebase=merges|interactive`, `--autostash`, `rebase.autoStash` are not implemented; `pull.ff=only` config is accepted
as "decided" but not applied; `rivet switch` with unmerged paths writes `c1: needs merge` to stderr where git writes it to stdout.

## Block 5 (autonomous run, base `6a816f8`)

Base gate re-derived before touching anything: fmt, clippy `-D warnings`, doc clean; `cargo test --workspace` 218 passed / 0 failed / 0 ignored;
`scenarios.sh` 152 PASS / `failures: 0`.

### Phase 0.1 — `log --graph` over several roots (landed, scenarios `s_log_graph_roots`, `s_log_graph_roots_chains`, `s_log_graph_roots_merges`, `s_log_graph_roots_random`)
Base: `rivet log --graph --all --oneline` over three lone roots exited 128 with `log: --graph over several root commits is not implemented (f77edd5 is a root that is not shown last)`
(git: `* f77edd5 r3`, `  * d052965 c2 b`, `* 5ddf3bd r1`). The bail is gone; `history.rs::root_shifts` decides how far right each root is drawn and `graph.rs` draws it.
The rule was derived by experiment (no `graph.c` source for 2.56 on this machine), then checked against ~3,000 random forests/merge histories
(`--oneline`, `%s`, `--format=medium`, `fuller`, `%n%b`, `--topo-order`, `-n`, `--skip`, bigger shapes with up to 6 roots and 4 merges); the last sweep had 0 differences. The rule:
- A root that is not the last row is *eligible* unless it was reached as the second or later parent of a merge shown above it (those are never moved).
- An eligible root is moved one column right when the next row is a new tip (not an existing column) and not itself a shifted root; otherwise it stays.
  A root that had a child above is led into its new column by a ` \ ` connector row (before it, after any collapsing rows).
- A root that opens a *new* column goes right of the blank columns left by the roots just above it, so a run of lone roots climbs one column per row (0,1,2,…) and the last row returns to 0;
  a root that continues an existing column leaves one blank column for the next row but does not accumulate.
- Columns right of a shifted root are pushed along with it; the blank separator row between entries is padded to the *unshifted* width.
Replaced the robustness test `log_graph_refuses_a_root_that_…` by `log_graph_draws_a_root_that_is_not_shown_last_one_column_to_the_right` (compares with git and pins the shifted shape).
Gate after: fmt, clippy, doc clean; 218 passed / 0 failed / 0 ignored; `scenarios.sh` 156 PASS / `failures: 0`.

Found while probing, NOT fixed (each reproduces without any root commit unless noted):
- `log --graph` with an octopus merge whose parents already sit in columns panics in `draw_octopus_merge` (`graph.rs`, index out of bounds):
  `z<-r1c0,r0c0; r0c1<-r0c0; m0 = merge(r1c0, r0c1, r0c0); m1 = merge(r0c1, r0c0)`, then `rivet log --graph --all --oneline`. Real git draws it.
- `log --graph --format='%h %s%n%b'` on a merge commit with an empty body: git draws the empty body line with graph columns (`| | `), rivet drops that line.
- `log --graph --date-order` and `--first-parent` with several tips: rivet's commit order differs from git's, so the drawing differs (ordering issue in the walk, not in the graph code).
- `rivet log` accepts a single revision only (`log a b` is `unexpected argument`).

### Phase 0.2 — `log` colour (landed, scenarios `s_log_color`, `s_log_graph_roots_color`, `s_log_decorate` extended)
Base: `rivet log --color=always …` died with `error: unexpected argument '--color' found` (exit 2) and `color.ui=always` was ignored, so
`git -c color.ui=always log --graph --oneline --decorate` and rivet differed on every line. Now `--color[=always|never|auto]`, `--no-color` (last one wins) and
`color.ui` (`always`/`never`/`auto`/boolean; `true`/`auto` paint only on a tty) paint exactly what git prints, byte for byte:
- the `commit <id>` line (medium/short/full/fuller) and the id of `--oneline`/`--pretty=oneline` in the commit colour (yellow), each followed by a plain reset;
- decorations piece by piece: `(`, `, `, `)` and ` -> ` in the commit colour, `HEAD` bold cyan, local branch bold green, remote branch bold red, `tag: ` and the tag name bold yellow, `refs/stash` bold magenta;
- the graph columns (their colours and the `\` connector row of a shifted root were already right; 0 differences over ~400 random coloured histories);
- `Author:`, `Date:`, `Merge:`, the message and `--format=…%d` (no `%C…` support) stay plain, as in git.
`color.diff.commit` and `color.decorate.{HEAD,branch,remoteBranch,tag,stash}` are honoured (names, `bright*`, 0–255, `#rrggbb`, `bold dim italic ul blink reverse strike`, `normal`, `reset`;
unit test `color_specs_match_what_git_prints` pins sequences taken from git). `--color=bogus` prints git's `error: option `color' expects …` and exits 129;
`color.ui=bogus` dies with `bad boolean config value 'bogus' for 'color.ui'`.
Found by the scenario and fixed on the way (red on the base against git): `--decorate` listed refs outside `refs/heads|remotes|tags|stash` (`refs/foo/bar`, `refs/notes/…`);
git decorates none of them, so now only those four namespaces decorate.
Known gaps: coloured diffs — `log --color=always -p|--stat|--name-*` dies with `log: coloured diff output is not implemented` rather than printing uncoloured diffs under a coloured header;
an invalid `color.*` value dies with `unable to parse '<key>'…` (git also names the file and line); negated attributes (`no-bold`) in a colour spec are refused.
Gate after: fmt, clippy, doc clean; 219 passed / 0 failed / 0 ignored; `scenarios.sh` 158 PASS / `failures: 0`.

### Phase 0.3 — octopus merge over shared columns no longer panics (landed, scenario `s_log_graph_octopus_shared`)
Base: `z <- r1c0, r0c0; r0c1 <- r0c0; m0 = merge(r1c0, r0c1, r0c0); m1 = merge(r0c1, r0c0)` then `rivet log --graph --all --oneline` panicked in
`draw_octopus_merge` (`index out of bounds: the len is 3 but the index is 3`); git prints `| | *-.   m0` / `| | |\ \`. `draw_octopus_merge` now counts the
columns actually added (`new_columns - columns`, as `graph.c` does) instead of `edges_added`, and clamps an index that falls outside the columns in use
(git reads memory beside its array there; only the colour of the `-.` depends on it). Uncoloured output matches git on 1,200+ octopus-heavy random histories;
the colour of the `-.` of an octopus whose parents already own columns can still differ (git reads uninitialised memory; ~25% of the most extreme coloured random histories).
Remaining difference seen with a seeded octopus-heavy history (1 in 300 uncoloured): the row between a commit line and the pre-commit expansion of the next octopus
prints `\` where git prints `|` (`output_pre_commit_line`, `prev_state == PostMerge` test); not investigated further.
Gate after: fmt, clippy, doc clean; 219 passed / 0 failed / 0 ignored; `scenarios.sh` 159 PASS / `failures: 0`.

### Phase 0.4 — `log --graph --format=…%n` after a merge or collapsing row (landed, scenario `s_log_graph_merges` extended)
Base: `--graph --format='%h %s%n%b'` (and any `tformat` ending in `%n`) over a merge commit lost one graph row that git prints: after the post-merge (or
collapsing) row the empty last message line used up, git's line terminator adds a padding row of its own once the commit's remaining graph rows are out
(`| | ` after `|\`, `|   ` after `|/`). The extended scenario failed on the base with exactly those rows missing (transcript diff: 6 rows, `| | `/`|   `).
Verified against ~700 random histories with `%s%n`, `%h%n%b`, `%h:%s%n%b---%n`, `--format=medium`, `--oneline`.
Known gap (pre-existing, not fixed): `--graph --format=format:%s` (separator, not terminator, semantics) over a merge draws an extra `| | ` row and glues the next
commit onto the previous text; `format:%s%n%b` matches. Not derived: git emits the separator after the next commit's graph update, rivet before.
Gate after: fmt, clippy, doc clean; 219 passed / 0 failed / 0 ignored; `scenarios.sh` 159 PASS / `failures: 0`.

### Phase 1.5 — `rm -r .` (landed, scenario `s_rm_whole_tree`, commit `dcea4e3`)
Base had the work committed from the handoff tree: `rm -r .` was refused and emptied directories were left behind; now it matches git (same `rm 'path'` lines and order, exit 0, work-tree root never unlinked).

### Phase 1.6 — `switch`/`checkout` unmerged path stream; `checkout --orphan` (landed, scenarios `s_switch_unmerged_stream`, `s_checkout_orphan_reflog`)
- `<path>: needs merge` goes to stdout (git), only `error: you need to resolve your current index first` stays on stderr. Probed with `/usr/bin/git` (index conflict with `MERGE_HEAD` removed; with `MERGE_HEAD` present `switch` instead dies `fatal: cannot switch branch while merging`, exit 128). Base transcript showed the line under `--- stderr` for `switch`, `checkout` and `switch --detach`.
- Orphan: git writes no reflog line at all (not in `logs/HEAD`, no `logs/refs/heads/<name>`); rivet wrote `checkout: moving from main to fresh`. Also found on the base: `checkout --orphan` emptied the index and work tree, but git keeps them (files show as `A  a`, `A  b`); only `switch --orphan` empties. Both fixed (`orphan_branch(.., empty_tree)`), and the following `checkout main` reflog label (`moving from second to main`) is compared.
Known gaps found while probing (not fixed): with unmerged paths and no `MERGE_HEAD`, `switch -c new` makes rivet print `M\tc1` and leave `MERGE_MSG`/`MERGE_MODE` behind, while git prints nothing extra and removes them; `rev-parse --abbrev-ref HEAD` is rejected by rivet's argument parser (`<ARG>...` required); `symbolic-ref` is not implemented.
Gate after: fmt, clippy, doc clean; 219 passed / 0 failed / 0 ignored; `scenarios.sh` 162 PASS / `failures: 0`.

### Phase 1.4 (update) — main `gc` pack write order (landed; supersedes the "NOT done" entry above; scenarios `s_gc_delta`, `s_gc_delta_shapes`, `s_gc_cruft`, `s_gc_cruft_expire`)
`packshape` in `scenarios.sh` no longer sorts: each pack is listed in `verify-pack -v` (storage) order, packs ordered by their sorted object set. On the base
`s_gc_delta` failed with git writing the tagged commit, then its tag, then the other commits, and with git writing a delta base followed by its whole delta family
(rivet interleaved the deltas of different bases); `s_gc_cruft` / `s_gc_cruft_expire` failed on blob/tree order.
Now ported (`git_write_order` in `gc.rs`, git's `compute_write_order`): untagged head of the list as listed, tagged tips (tag refs and what they peel to), remaining commits/tags,
trees, then each unwritten object with its delta family (root, children before grandchildren). Findings from the oracle: an object stored as a delta in a pack being
replaced counts as neither commit/tag nor tree there (git still sees its delta type), so reused tree deltas wait for their family; the object list puts tag objects after
all commits (pending-list order), then the index's blobs (`--indexed-objects`), then trees.
Caveat (approximation, not exact): git also lists the index's *cache-tree* trees ahead of every commit's tree. Rivet does not keep the `TREE` extension, so it treats the
index as cached only when it equals `HEAD`'s tree exactly (HEAD read from the walk tips, as the ref store snapshot is stale after `pack_refs`). Partially valid caches
(some directories invalidated) are not reproduced; no scenario hits that.
Gate after: fmt, clippy, doc clean; 221 passed / 0 failed / 0 ignored (2 new unit tests for the ordering); `scenarios.sh` 162 PASS / `failures: 0`; the five gc scenarios passed 3 runs in a row.

### Phase 2.1 — `pull --rebase` fork point, `merge-base --fork-point`, fetch note spacing (landed, scenarios `s_pull_rebase_fork_point`, `s_merge_base_fork_point`)
Base, `s_pull_rebase_fork_point` (upstream rewrote `two` after two local commits were made on top of it): rivet rebased `--onto <fetched tip> <fetched tip>`, so the old `two` was
replayed (`Rebasing (1/3)…`, log had a stray `two` commit, `ls-files` had `b`); git replays only `local1`, `local2` (`Rebasing (1/2)`). The same scenario showed fetch printing
`(forced update)` after one space where git prints two (`main       -> origin/main  (forced update)`); push keeps one. `fetch_line` now separates a note with two spaces.
`merge-base --fork-point <ref> [<commit>]` is implemented as git's `get_fork_point` (candidates = first reflog entry's old value + every entry's new value, or the ref itself when it has no
reflog; exactly one best common ancestor of the commit and all of them, and it must be one of the candidates). `pull --rebase` computes it before the fetch from
`refs/remotes/<remote>/<branch>` and uses it as the rebase upstream. The merge-base usage text now includes git's option list. `--fork-point` also accepts `-a/--all` like git (ignored).
Not done: the fork point is only computed when `pull` has no explicit refspec arguments and the remote is the branch's configured one; `git pull --rebase <remote> <branch>` still rebases onto the
fetched tip alone. The tracking ref is taken from the default `+refs/heads/*:refs/remotes/<remote>/*` mapping, not from a customised `remote.<name>.fetch`.
Gate after: fmt, clippy, doc clean; 221 passed / 0 failed / 0 ignored; `scenarios.sh` 164 PASS / `failures: 0`.

### Phase 2.2 — `--autostash` for `rebase`, `merge`, `pull`; `checkout HEAD` (landed, scenarios `s_rebase_autostash`, `s_merge_autostash`, `s_pull_autostash`, `s_checkout_head`)
Base: `pull --autostash` died `pull: --autostash, --interactive and --gpg-sign are not implemented`; `rebase --autostash` and `merge --autostash` were `unexpected argument`.
Behaviour taken from `/usr/bin/git`: only tracked changes (index and work tree) are stashed; stdout `Created autostash: <7-hex>` (stash commit `On <branch>: autostash`, not stored); on success stderr
`Applied autostash.` (changes come back unstaged, no stash entry ever existed); when applying conflicts the commit is stored as stash entry `autostash`, the 5-line "Your local changes are stashed, however applying them…" text goes to stderr, markers read `Updated upstream` / `Stashed changes`,
and the operation still succeeds. `rebase` keeps the commit in `.git/rebase-merge/autostash` and applies it on finish, `--continue` finish and `--abort`; up-to-date rebases stash and unstash. `merge` creates it right before the fast-forward / merge
(after `Updating a..b`), and a conflicted merge leaves `.git/MERGE_AUTOSTASH` plus `When finished, apply stashed changes with \`git stash pop\``; `commit` / `merge --continue` / `merge --abort` apply it, `merge --quit` and `reset` store it
(`Autostash exists; creating a new stash entry.…`). `rebase.autoStash` / `merge.autoStash` and `--no-autostash` work; `pull --rebase --autostash` skips the clean-tree refusal and passes the flag on, and a fast-forwarding `pull --rebase` goes through the merge code as before.
Reflog action of a `pull`-driven rebase is now the whole command line (`pull --rebase --autostash (pick): …`), as git's `GIT_REFLOG_ACTION`.
Found with the new pull scenario: `checkout HEAD` / `checkout @` detached `HEAD` and wrote a reflog line; git treats it as no move at all (nothing printed but `M\tpath` lines, no reflog entry, attached stays attached; `-f` resets the tree). Fixed.
Not done: `merge --autostash` with `--squash` or `--no-commit` and `pull --autostash --squash` are refused with an explicit "not implemented" error; `rebase --autostash` with `--interactive`/`--root` does not exist yet (those are not implemented at all).
Gate after: fmt, clippy, doc clean; 221 passed / 0 failed / 0 ignored; `scenarios.sh` 168 PASS / `failures: 0`.

### Phase 2.3 — rebase `--quit` / `--show-current-patch`, unmerged-path hint in `status` (landed, scenario `s_rebase_conflict_kinds`)
Base: `rebase --quit` and `--show-current-patch` were `unexpected argument`; `status` during a rebase (and a merge) with a `deleted by us/them` path said `(use "git add <file>..." to mark resolution)` where git says
`(use "git add/rm <file>..." as appropriate to mark resolution)` (`git rm` alone when every path is deleted on both sides). `--quit` forgets `.git/rebase-merge` and leaves `REBASE_HEAD`, `MERGE_MSG`, `AUTO_MERGE` (checked against git);
`--show-current-patch` is `show REBASE_HEAD`. The scenario also covers modify/delete, add/add and a rename with an edit on the other side.
Gate after: fmt, clippy, doc clean; 221 passed / 0 failed / 0 ignored; `scenarios.sh` 169 PASS / `failures: 0`.

### Phase 3.1 — `pack-refs`, `repack` (landed, scenarios `s_pack_refs`, `s_repack`, `s_repack_keep_multi`)
Base: neither command existed (`error: unrecognized subcommand`).
- `pack-refs [--all] [--no-prune] [--include/--exclude <pattern>]`: tags by default, everything with `--all`, refs already in `packed-refs` stay, peeled `^` lines for tag objects, loose files pruned unless `--no-prune` (checked: layout of `refs/` and the `packed-refs` text after each step, in the transcript). `--auto` is refused ("not implemented").
- `repack [-a|-A] [-d] [-k] [-f|-F] [-l] [-n] [-q]` (`rivet_core::gc::repack`, new `RepackOptions`/`RepackReport`): the plain form packs the reachable objects that are in no pack yet (reachability = refs, reflogs, index, as `repack` passes `--all --reflog --indexed-objects`); `-a`/`-A` repack everything not `.keep`; `-d` removes replaced packs and runs the equivalent of `prune-packed` (loose objects some pack holds, then empty fan-out dirs); `-A` writes the unreachable objects of replaced packs back out loose; `-k` appends them to the new pack. `Nothing new to pack.` goes to stdout unless `-q`. Pack bytes, order and delta shapes are compared with `packshape`, which now asserts storage order (Phase 1.4).
- To match, the object list now follows git more closely: the index's *cache-tree* directories are approximated per directory (a directory counts as valid when everything the index holds below it equals `HEAD`'s subtree; git keeps untouched directories valid after an `add`), walked in pending-list order ahead of every commit's tree. This replaced the all-or-nothing `HEAD == index` test of Phase 1.4; `s_gc_delta_shapes`, `s_gc_cruft` and the repack scenarios exercise both a clean and a partly staged index.
- Side findings fixed on the way: `branch -D -q` printed `Deleted branch …` (git is silent with `-q`). The harness now writes `maintenance.auto = false` and `gc.auto = 0` into the per-run global config: git 2.56's own post-command auto maintenance wrote a multi-pack-index and a cruft pack in the middle of `s_repack_keep_multi`.
Known gaps: `repack -k` lists the unreachable objects of *several* replaced packs in an order I could not reproduce (git's order is neither pack mtime, directory listing nor name; single-pack `-k` and loose ones match, see `s_repack`); `--cruft`, `--cruft-expiration`, `-b`, `--write-midx`, `--geometric`, `--window/--depth`, `--max-pack-size`, `--keep-pack` are not accepted; `pack-refs --auto` and `repack`'s `update-server-info` step are not done.
Gate after: fmt, clippy, doc clean; 221 passed / 0 failed / 0 ignored; `scenarios.sh` 172 PASS / `failures: 0`.

### Phase 3.2 — `bisect` start/good/bad/skip/next/run/replay/log/terms/reset (landed, scenarios `s_bisect_basic`, `s_bisect_start_args`, `s_bisect_terms`, `s_bisect_run`)
Base: `bisect` did not exist. The half-written implementation found in the tree (`cmd/bisect.rs`, CLI wiring, four scenarios) had two red scenarios, both fixed against real git:
- `s_bisect_start_args`: `bisect bad 1 2` without a bisection in progress printed `error: 'git bisect bad' can take only one argument.`; git checks "started" first and prints `You need to start by "git bisect start"` (exit 1).
- `s_bisect_run`: `bisect replay <log>` replays `start` with its auto-next step (prints `Bisecting: …`, checks out the midpoint, writes `BISECT_EXPECTED_REV`) and the `good`/`bad` lines with refs and log only, then runs the final next-step. rivet skipped the first and cleared `BISECT_EXPECTED_REV`/`BISECT_ANCESTORS_OK` while replaying.
Also fixed two clippy errors in the file (`format!` inside `println!`, manual range check).
Known gaps (each refuses with an explicit "not implemented" error): `bisect start --first-parent`, `--reset-when-found`, and path arguments after `--`; `bisect visualize`/`view` and `bisect --no-checkout` beyond writing `BISECT_HEAD` are not covered by a scenario.
Gate after: fmt, clippy, doc clean; 221 passed / 0 failed / 0 ignored; `scenarios.sh` 176 PASS / `failures: 0`.

### Phase 3.3 — `cherry` (landed, scenario `s_cherry`)
Base: `cherry` was `unrecognized subcommand`. New `cmd/cherry.rs`: `cherry [-v] [<upstream> [<head> [<limit>]]]` lists the non-merge commits of `<upstream>..<head>` (minus `<limit>`'s history), oldest first, `-` when the patch-id also occurs among the non-merge commits of `<head>..<upstream>`, else `+`; `-v` appends the subject. It reuses `rebase`'s patch-id (whitespace-squeezed changed and context lines plus paths and modes; equality is what matters, the hash value is not git's). With no upstream argument it uses the branch's tracked upstream; without one (or with four or more arguments) it prints `Could not find a tracked remote branch, please specify <upstream> manually.` plus git's usage text and exits 129; unknown revisions are `fatal: unknown commit <rev>` / 128 (all checked against `/usr/bin/git`).
The scenario covers cherry-picked (`-`) and unique (`+`) commits in both directions, `-v`, a limit, explicit and tracked upstream, a merge commit on the head side, and the error paths.
Found with the scenario: `branch -q --set-upstream-to=<x>` printed `branch '<b>' set up to track '<x>'.`; git is silent with `-q`. Fixed.
Process note: a first version of the scenario was appended after the runner loop at the end of `scenarios.sh`, so it never ran and "passed" with two empty transcripts. Scenarios must be added above the `if [ $# -eq 0 ]` runner block.
Known gaps: `--abbrev[=<n>]` is not accepted; a subject spanning several lines is shown as its first line, where git joins the paragraph with spaces.
Gate after: fmt, clippy, doc clean; 221 passed / 0 failed / 0 ignored; `scenarios.sh` 177 PASS / `failures: 0`.

### Phase 3.4 — `worktree` add/list/lock/unlock/remove/prune, and linked work trees in the core (landed, scenarios `s_worktree_linked`, `s_worktree_basic`, `s_worktree_lock_remove`, `s_worktree_prune`, `s_worktree_branch_guards`, `s_worktree_force_guards`, `s_worktree_gc_keeps`)
Base: no `worktree` command, and `Repository` did not follow `commondir`: a linked work tree made by real git opened with an empty object store.
- Core: `Repository::common_dir()` (new, documented) and `RefStore::open_linked`. `open_with` reads `commondir`; objects, config, `info/exclude` and `packed-refs` come from the common directory, the index, `HEAD`, `MERGE_HEAD`, `rebase-merge/` and other state stay in the work tree's own directory, and the work tree path comes from its `gitdir` file. The ref store routes `HEAD`, other pseudo-refs and `refs/bisect|worktree|rewritten/` (and their logs) to the work tree's directory and everything else to the common one, as git does. `gc`, `repack`, `pack-refs`, `stash`, `config` writes and `Repository::should_log_ref` use the common directory.
- `worktree add [-f] [-d] [--[no-]checkout] [--lock [--reason]] [-b|-B <branch>] [-q] <path> [<commit-ish>]`, `list [-v | --porcelain [-z]]`, `lock [--reason]`, `unlock`, `remove [-f [-f]]`, `prune [-n] [-v] [--expire now]`. Checked against `/usr/bin/git`: the layout of `.git/worktrees/<id>` (`gitdir`, `commondir`, `HEAD`, `ORIG_HEAD`, `locked`, `logs/HEAD` with git's two entries, `refs/`), id collisions (`w1` then `w11`), the stream of every message (`Preparing worktree …` on stderr, `HEAD is now at …` on stdout), exit codes (255 for an existing `-b` branch), usage texts, path order of `list`, column alignment, `locked`/`prunable` in both modes.
- Other commands that had to learn about linked work trees, each from a scenario diff: `branch` marks branches checked out elsewhere with `+`; `branch -d/-D`, `branch -f`, `branch -M` onto, `switch`/`checkout` of, and `switch -C` of a branch used by another work tree are refused with git's wording; `branch -m` moves the `HEAD` of every work tree on the branch and writes git's `HEAD` log lines (also for the current work tree, which was missing before); a detached `HEAD` with no `checkout:` log entry prints `(no branch)` / `Not currently on any branch.`; `switch --detach <bad>` says `invalid reference`.
- Data safety, found by `s_worktree_gc_keeps`: `gc` in one work tree deleted a blob that only another work tree's index referenced (`fsck` then reported `missing blob`). Reachability for `gc`, `repack` and `fsck` now includes the other work trees' detached `HEAD`, index and `HEAD` log, and `gc`'s reflog expiry covers their `HEAD` logs as `git reflog expire --all` does. The scenario fails with the fix disabled (checked).
Known gaps: `worktree move`, `repair`, `add --orphan`, `--track`, `--guess-remote`, `--relative-paths`, `list` of a bare main repository (written, not exercised), `prune --expire <date>`, `branch -vv` showing the work tree path; per-work-tree config (`config.worktree`) and `core.worktree` for linked work trees; the pack object order of `gc`/`repack` does not take other work trees' `HEAD` into account (the objects are kept, but their position may differ from git's); `rev-parse --git-common-dir` is not in the frozen grammar. Found and left alone because they are not about work trees: `cat-file -p :<path>` fails with `empty revision`, `checkout -B <name>` without a start point is a clap error, `switch -C <current>` prints `Switched to and reset branch` where git prints `Reset branch`, and `log HEAD@{1}` on an empty reflog says `ambiguous argument` where git says `log for HEAD is empty`.
Gate after: fmt, clippy, doc clean; 221 passed / 0 failed / 0 ignored; `scenarios.sh` 184 PASS / `failures: 0`.

### Phase 3.5 — `archive` tar / tar.gz / zip (landed, scenarios `s_archive_tar`, `s_archive_targz`, `s_archive_zip`, `s_archive_add_file`)
Base: `archive` did not exist. New `rivet_core::archive` (`archive`, `ArchiveOptions`, `ArchiveFormat`, `ArchiveFile`, documented) and `cmd/archive.rs`.
- `archive [--format=tar|tar.gz|tgz|zip] [-o <file>] [--prefix=<p>] [--add-file=<f>] [--mtime=<secs>] [-0..-9] [-v] [-l] <tree-ish> [<path>...]`. The format follows the output name when `--format` is absent. `--prefix` applies to what follows it, so it also decides the archive path of a later `--add-file` (basename under the prefix in effect). Run from a subdirectory it archives that subdirectory, with the commit's time and id, as git does.
- Byte-for-byte equal to `/usr/bin/git` (compared by `sha1sum` of the file in the scenarios, and directly with `cmp` on a second repository): tar with the pax global header, tree order with directory entries, prefix directory entry (only for a prefix ending in `/`), parents of path-limited entries, ustar `prefix` split and pax `path`/`linkpath` for long names, symlinks, executables (`0775`/`0664` from `tar.umask=002`), gitlinks as directories, record padding to 10240; zip with stored entries (`-0`), the commit id as archive comment, extended timestamp, DOS time in the local zone, UTF-8 flag. Error texts and exit codes (unmatched pathspec before any output, unknown format, bad tree-ish, usage, `File not found: …`) match too.
- `export-ignore` is read from the tree's `.gitattributes` files (any depth, last match wins) and `info/attributes`; pathspecs are literal paths, globs where `*` crosses `/`, and `:(exclude)` / `:!` / `:^`.
- Compressed bytes are not git's: gzip output is checked by header and by what it decompresses to, zip by `zipinfo` fields (without sizes) and each member's content. A first run with the check on the whole file showed the difference (git deflates with zlib, `flate2` here uses its Rust backend; git's tar.gz comes from `gzip -cn`).
Known gaps: `export-subst` is refused with an error where git expands `$Format:…$`; other pathspec magic; `--remote`, `--exec`, `--worktree-attributes`, `--add-virtual-file`; `--mtime` with anything but seconds; zip64 (65 535 entries or 4 GiB members are refused); attributes from `core.attributesFile` and `[attr]` macros; `-v` names entries before writing instead of while writing.
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored (five new unit tests in `archive`); `scenarios.sh` 188 PASS / `failures: 0`.

### Phase 3.6 — `submodule status`, `.gitmodules` read-only; `describe --all` (landed, scenarios `s_submodule_status`, `s_describe_all`)
Base: no `submodule` command; `describe` had no `--all`. `submodule [status] [-q|--quiet] [--cached] [--recursive] [--] [<path>...]` (a bare `submodule` is `status`) in `cmd/submodule.rs`; every other subcommand is refused by name ("not implemented (only 'status' is)").
- Per gitlink of the index, in index order: ` <commit> <path> (<description>)`, `+` when the submodule's `HEAD` is not the recorded commit (then the checked-out commit is shown, the recorded one with `--cached`), `-<recorded commit> <path>` when it is not checked out (no description), `U000… <path>` for a conflict. `--quiet` prints nothing; `--recursive` descends with the path prefix. Unknown pathspec: `error: pathspec 'x' did not match any file(s) known to git`, exit 1; a gitlink without a `.gitmodules` mapping: `fatal: no submodule mapping found in .gitmodules for path 'x'`, exit 128. `.gitmodules` is read from the work tree, then the index, then `HEAD` (found with the scenario: git still reports a submodule after `git rm --cached .gitmodules`).
- The description is git's chain `describe`, `describe --tags`, `describe --all --always`. `describe` moved into `describe_commit` (returns the name or the failure text) so both commands share it, and gained `--all` (any ref, named `heads/main`, `tags/v1`, `remotes/origin/main`; priority annotated tag, lightweight tag, other ref), which `s_describe_all` compares with git on exact, nearest-ancestor, `--long`, `--exact-match`, detached and tag-deleted cases.
Known gaps: the `describe --contains` step (naming a commit by a later tag) is refused with an error when such a tag exists; `init`, `update`, `add`, `foreach`, `sync`, `summary`, `deinit`, `absorbgitdirs` and the rest are not implemented; submodule `HEAD` found through `.git` files and `core.worktree` only as `Repository::open_or_discover` does; `submodule.<name>.active`/`ignore` settings are not consulted.
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored; `scenarios.sh` 190 PASS / `failures: 0`.

### Phase 3.7 — `worktree move` and `worktree repair`, including a moved main repository (landed, scenarios `s_worktree_move`, `s_worktree_repair`, `s_worktree_repair_main`)
Base `ca573eb`: both subcommands were refused by name; the three new scenarios fail there (`rivet` exits with `worktree: 'move' is not implemented`, git exits 0 or prints its own diagnostics).
- `move [-f [-f]] <worktree> <new-path>`, checked against `/usr/bin/git` 2.56: an existing directory target receives the work tree under its basename (`../dn` -> `../dn/<basename>`); a target that exists and is not an empty directory is `fatal: '<dst>' already exists` (checked before the lock); a locked work tree needs `-f -f` (`cannot move a locked working tree[, lock reason: …]` + `use 'move -f -f' …`); the main work tree is `'<spec>' is a main working tree`; the source is validated (`'<p>/.git' does not exist`, `… is not a .git file, error code N`, `… does not point back to '.git/worktrees/<id>'`); a failed `rename` is `failed to move '<src>' to '<dst>': <strerror>`. The id under `.git/worktrees/` is **not** renamed (git keeps `worktrees/d` for a work tree that now lives at `d2`). Afterwards `worktrees/<id>/gitdir` and the work tree's `.git` file are rewritten. Moving the current linked work tree is allowed (git allows it).
- `repair [<path>...]`: each path first (not a directory: `error: not a valid path: <arg>`; unreadable `.git`: `error: unable to locate repository; .git file broken|does not reference a repository: <p>/.git`, exit 1; a stale target is resolved through its last component under this repository's `worktrees/`, which is how a work tree moved together with the main repository is found; `repair: gitdir incorrect|unreadable: <admin>/gitdir`), then every registered work tree whose `.git` file is missing, broken or points elsewhere (`repair: .git file broken|incorrect: <path>`), in directory order as git does. Repairing from the new location of a moved main repository fixes all `.git` files.
- Opening a work tree whose `.git` file names a missing directory now says `fatal: gitfile does not point to a valid repository: <p>/.git` (was `path escapes the repository root`); mapped in `cmd::open`, the core is unchanged.
Not done / differs from the brief: git has no "main work tree is bare" or "current work tree" refusal for `move` and no dirty-tree check (the usage text says "even if dirty" but only the lock is checked), so none was added. `--relative-paths` is refused by name. Run from a subdirectory, the path in `failed to move` is the argument as typed, as in git at the top level only. The scenarios avoid `rev-parse --git-dir` / `--git-common-dir` (not in the frozen grammar).
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored; `scenarios.sh` 193 PASS / `failures: 0`.

### Phase 3.8 — `submodule init` (landed, scenario `s_submodule_init`)
Base `44a8b02`: `submodule init` was refused by name (`submodule: 'init' is not implemented (only 'status' is)`).
- `submodule [-q] init [-q] [--] [<path>...]`, checked against `/usr/bin/git` 2.56 (clones of a superproject built with real git): per selected gitlink (cwd-relative pathspecs, index order) it sets `submodule.<name>.active = true` when the submodule is not active yet (per-submodule `active`, then `submodule.active` pathspecs, then an existing url), then `submodule.<name>.url` when unset — in that order, `active` first — and `submodule.<name>.update` when unset and `.gitmodules` has one. `./` and `../` urls are resolved against `remote.<branch's remote or origin>.url` (git's `relative_url`: strips one component per `../`, handles `host:path`), or against the work tree path with `warning: could not look up configuration 'remote.origin.url'. Assuming this repository is its own authoritative upstream.` when there is no remote. `Submodule '<name>' (<url>) registered for path '<path>'` goes to **stderr** (path shown relative to the cwd, `../x` from a subdirectory), silent with `-q`.
- Errors: unmatched pathspec `error: pathspec '…' did not match any file(s) known to git` (1); gitlink with no `.gitmodules` entry or no url `fatal: No url found for submodule path '…' in .gitmodules` (128, earlier submodules stay written); an `update` other than checkout/merge/rebase/none in `.gitmodules` (`!cmd`) `fatal: invalid value for 'submodule.<name>.update'` (128); an unknown option prints the whole `git submodule` usage and exits 1. With `submodule.active` set and no pathspec, only active submodules are initialised (found with the scenario: `submodule.<name>.active = false` is left untouched).
Known gaps: `submodule.active` pathspec magic/negation beyond what `pathspec_matches` does; the `update = none`/`command` warning path (`warning: command line update mode suggested…`) is unreachable because git refuses `!cmd` in `.gitmodules` first.
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored; `scenarios.sh` 194 PASS / `failures: 0`.

### Phase 3.9 — `submodule sync` (landed, scenario `s_submodule_sync`)
Base `3cc4caf`: refused by name. `submodule [-q] sync [-q] [--recursive] [--] [<path>...]`, compared with `/usr/bin/git` 2.56 on a clone with a nested submodule (`vendor/lib` and `other`, each with `in`), `.gitmodules` urls edited after the clone:
- Only active submodules are touched (an uninitialised one is skipped silently — found with the scenario). Per submodule: `Synchronizing submodule url for '<path>'` on **stdout** (silent with `-q`), `submodule.<name>.url` rewritten in the superproject, and, when the submodule is checked out, `remote.<its default remote>.url` in `.git/modules/<name>/config` (the remote of its branch, else `origin`; created when it was removed). `./`/`../` urls are resolved against the superproject's remote silently (no "Assuming this repository…" warning here, unlike `init`); the url written into the submodule gets `../` for every path component when the superproject's remote is itself relative (`../../../lib2` for `vendor/lib`). `--recursive` descends with the path prefix (`vendor/lib/in`); from a subdirectory the paths are cwd-relative (`../other`).
- Errors as for `init`: unmatched pathspec (1), unknown option prints the full `git submodule` usage (1), `fatal: No url found for submodule path '…' in .gitmodules` (128).
Known gaps: `submodule.<name>.url` of a *nested* superproject uses the nested work tree's own remote for relative urls (as git does) but is only exercised with absolute remotes below the first level.
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored; `scenarios.sh` 195 PASS / `failures: 0`.

### Phase 3.10 — `submodule add` (landed, scenarios `s_submodule_add`, `s_submodule_add_urls`)
Base `110f102`: refused by name. `submodule [-q] add [-q] [-f] [-b <branch>] [--name <n>] [--] <repository> [<path>]`, compared with `/usr/bin/git` 2.56:
- Clone: `.git/modules/<name>` (name = `--name` or the path) holds the git directory (`core.bare=false`, `logallrefupdates`, relative `core.worktree`, `remote.origin`, `branch.<b>`, reflogs `clone: from <url>`, packed refs), `<path>/.git` is `gitdir: <relative path>`. As in git it is a `clone --no-checkout` followed by a checkout, so a failed `-b nosuch` leaves the module directory and an empty work tree (`fatal: 'origin/nosuch' is not a commit …` / `fatal: unable to checkout submodule …`, 128). With `-b` the branch is created from `origin/<b>` with git's reflog lines (`branch: Created from origin/dev`, `checkout: moving from main to dev`), tracking config, and `branch = <b>` goes into `.gitmodules`.
- Then: gitlink staged at the submodule's `HEAD`, a `[submodule "<name>"]` section (`path`, `url` as typed, `branch`) appended to `.gitmodules` as text (comments in an existing file survive; the whole file is staged), and `submodule.<name>.url` (relative `./`/`../` urls resolved against the superproject's remote or location) then `active = true` in `.git/config`.
- Checks and messages (all compared): `'<p>' already exists in the index`; `'<p>' already exists and is not a valid git repo`; ignored path (`The following paths are ignored by one of your .gitignore files:` + the two `hint:` lines unless `advice.addIgnoredFile=false`, exit 1; `-f` overrides); an existing git directory under `.git/modules/<name>` without `--force` (the multi-line "A git directory for … is found locally with remote(s)" text); `Adding existing repo at '<p>' to the index` for a work tree that already is a repository (no clone); `repo URL: '…' must be absolute or begin with ./|../`; `Relative path can only be used from the toplevel of the working tree`; `'…' is outside repository at '…'`; empty path; failed clone (`fatal: repository '<url>' does not exist` / the `file://` "does not appear to be a git repository" block, then `fatal: clone of … into submodule path '<abs>' failed`, with the created directories removed); `Cloning into '<abs path>'...` / `done.` on stderr (`done.` only for plain local paths, not `file://`); option order (options after the first positional print the `add` usage, 129; an unknown option before it prints the whole `git submodule` usage, 1).
- `clone` itself was split (`populate_clone`, with a `checkout` flag) so the submodule clone reuses the fetch/ref setup.
Known gaps (each refuses with an explicit error): `--depth`, `--reference`, `--dissociate`, `--ref-format`, `--progress`, and `--force` onto an existing `.git/modules/<name>` (git prints "Reactivating local git directory" and re-checks it out); only local-path and `file://` sources are exercised. The scenarios compare config, HEAD, refs, reflogs and packed-refs of the module directory, not its `description`/`hooks`/`info` files or object layout.
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored; `scenarios.sh` 197 PASS / `failures: 0`.

### Phase 3.11 — `submodule update` (landed, scenarios `s_submodule_update`, `s_submodule_update_recursive`)
Base `8eabf22`: refused by name. `submodule [-q] update [--init] [--recursive] [-f] [-q] [-N] [--checkout] [--] [<path>...]`, compared with `/usr/bin/git` 2.56 on a clone whose submodules (each with a nested one) start unregistered:
- Per gitlink of the index, in order: not active → silent without pathspecs, with explicit paths `Submodule path '<p>' not initialized` / `Maybe you want to use 'update --init'?` (both on **stderr**); `submodule.<name>.update = none` → `Skipping submodule '<p>'` (stderr); other strategies (`merge`, `rebase`) and `--remote`/`--merge`/`--rebase`/`--depth`/`--reference`/`--filter`/`--single-branch`/`-j` are refused with an explicit "not implemented". `--init` runs the `init` logic first (shared `init_in`, so the `registered for path` lines come first).
- A missing work tree is cloned into `.git/modules/<name>` (same helper as `add`: `Cloning into '<abs>'...` / `done.` on stderr), or re-attached when the module directory already exists (`.git` file + `core.worktree`); a fresh module is always checked out, which prints `Submodule path '<p>': checked out '<oid>'` (stdout, silent with `-q`) and writes the `clone: from` + `checkout: moving from main to <oid>` reflog lines. An existing module is only touched when `HEAD` differs from the recorded commit (or with `-f`): fetch from `origin` first when the commit is missing (git's `From …` report, reflog `fetch` / `fetch --quiet`; refused with `unable to read tree` / exit 128 under `-N`), then a detached checkout; local changes in the way give git's `error: Your local changes to the following files would be overwritten by checkout:` block plus `fatal: Unable to checkout '<oid>' in submodule path '<p>'` (exit 1); `-f` discards them. Re-checking out the commit `HEAD` already detaches at writes no reflog line (found with the scenario).
- `--recursive` descends with the path prefix (`other/in`), including a freshly cloned submodule's own submodules; with `--init` the nested ones are registered and cloned too.
- Core fix found by the scenario: `reset_hard_to_tree` (used for the fresh checkout, `reset --hard`, `checkout -f`) now creates the empty directory where a gitlink sits, as git does; before, a gitlink left no directory and `status` reported it deleted.
Not covered / gaps: `Fetched in submodule path …, but it did not contain …` (the direct-fetch fallback for an unadvertised commit) is implemented as a plain refusal and not compared; `status` of the superproject does not report a submodule whose `HEAD` moved (` M vendor/lib`) — a `status` gap that existed before this unit (the scenarios use `submodule status` instead); `checkout`/`switch` into a tree with a gitlink still do not create its directory (only the `reset --hard` path does).
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored; `scenarios.sh` 199 PASS / `failures: 0`.

### Phase 3.12 — `submodule deinit` (landed, scenario `s_submodule_deinit`)
Base `31feb5c`: refused by name. `submodule [-q] deinit [-q] [-f] (--all | [--] <path>...)`, compared with `/usr/bin/git` 2.56 on a clone with checked-out submodules (one with a nested submodule) and one without:
- Arguments: no path and no `--all` → `fatal: Use '--all' if you really want to deinitialize all submodules` (128); `--all` with paths → `error: pathspec and --all are incompatible` + the `deinit` usage (129); unknown option → the whole `git submodule` usage (1); unmatched pathspec as for the others (1).
- Per gitlink: unless `-f`, a submodule that has local modifications (modified or untracked files, or `HEAD` away from the recorded commit) or whose gitlink is staged differently from `HEAD` is refused with `error: the following file has local modifications:` / `has changes staged in the index:` + the path + `(use --cached to keep the file, or -f to force removal)` and `fatal: Submodule work tree '<p>' contains local modifications; use '-f' to discard them` (128). Otherwise: `core.worktree` is removed from `.git/modules/<name>/config` (`warning: Could not unset core.worktree setting in submodule '<name>'` when it was not there, preceded by `error: could not lock config file .git/modules/<name>/config: No such file or directory` when the module directory does not exist), the work tree is emptied but kept (`Cleared directory '<p>'`, stdout, also for a submodule that was never cloned), and `[submodule "<name>"]` is dropped from `.git/config` with `Submodule '<name>' (<url from .gitmodules>) unregistered for path '<p>'` only when that section existed. `.git/modules/<name>` itself stays, so `update --init` afterwards re-attaches it (covered).
Gaps: git's `has staged content different from both the file and the HEAD` variant (gitlink staged, committed state different from both) is not distinguished; the check is a single-pass approximation of `git rm -n`.
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored; `scenarios.sh` 200 PASS / `failures: 0`.

### Phase 3.13 — `submodule foreach` (landed, scenario `s_submodule_foreach`)
Base `4392929`: refused by name. `submodule [-q] foreach [-q] [--recursive] [<command> [<arg>...]]`, compared with `/usr/bin/git` 2.56:
- Every checked-out submodule (has a usable `.git`, in index order; not the active filter) prints `Entering '<displaypath>'` (stdout, not with `-q`/`--quiet`) and runs the command through `sh -c '<command> "$@"' <command> <arg>...` in its work tree — git's own shape, which is why `foreach 'echo "$@"' x` prints `x x` — with `name`, `sm_path`, `displaypath`, `sha1`, `toplevel` and `GIT_DIR=.git` in the environment and `path` as a shell variable (not exported, as in git). `--recursive` descends after the command with the display prefix (`other/in`; `toplevel` and `path` relative to the nested superproject). Run from inside a submodule it works on that repository's own submodules.
- A failing command stops the walk: `fatal: run_command returned non-zero status for <displaypath>` followed by a line holding a single `.` (git's text), exit 128; failures below `--recursive` add `… while recursing in the nested submodules of <displaypath>`. No command is allowed (just the `Entering` lines); an unknown option, `--`, prints the whole `git submodule` usage (1).
Gaps: the child's stdout/stderr are inherited (not captured), so `-q` only affects our own `Entering` line; no `submodule.<name>.ignore`/activity filtering, as in git.
Gate after: fmt, clippy, doc clean; 226 passed / 0 failed / 0 ignored; `scenarios.sh` 201 PASS / `failures: 0`.

### Phase 3.14 — block summary for Items 0 and 1
`worktree move`/`repair` (3.7) and `submodule init`, `sync`, `add`, `update`, `deinit`, `foreach` (3.8–3.13) are in. Still refused by name: `submodule summary`, `absorbgitdirs`, `set-branch`, `set-url`. Unrelated gaps found on the way and left alone: superproject `status` does not report a submodule whose `HEAD` moved; `checkout`/`switch` do not create the directory of a gitlink; `rev-parse --git-dir` without an argument and `--git-common-dir` are not in the frozen grammar.

### Phase 3.15 — `bundle` create / verify / list-heads / unbundle, and `fetch` from a bundle (landed, scenario `s_bundle_roundtrip`)
Base `938b722`: no `bundle` command (`unrecognized subcommand`), and `fetch <file>` failed with "not a repository". New `rivet_core::bundle` (`Bundle`, `Prerequisite`, `BundleRef`, `header`, `looks_like_bundle`, documented; three unit tests) plus a private `BundleTransport` that `open_transport` picks for a `file` url that is a bundle file, and `cmd/bundle.rs`.
- `bundle create [-q] [--version=2|3] <file|-> <rev-list-args>`: positive revisions, `^X`, `A..B`, `--all`, `--not`; refs in the header are the ones the positive arguments name (`main` → `refs/heads/main`, tags with the tag object's id, `HEAD`; `--all` = refs by name then `HEAD`); prerequisites are the `--boundary` commits in git's order with their subjects; version 3 writes `@object-format=sha1`. Errors: `Refusing to create empty bundle.` (no refs or no commits, 128), `need a <file> argument` + usage (129), unknown revision, `unsupported bundle version N`. **The pack is written undeltified**, so the bundle file is not byte-identical to git's; the header (compared byte for byte), the object set and acceptance by git are: the scenario runs `git bundle verify`/`list-heads` on every bundle rivet wrote, and `git fetch`, `git bundle unbundle` and `git fsck --strict` on them.
- `verify [-q] <file>`: `<file> is okay` on stderr, then (stdout) the refs, `The bundle records a complete history.` or `The bundle requires this ref:` (plural forms with counts), and the hash algorithm; missing prerequisites give `error: Repository lacks these prerequisite commits:` + `error: <id> ` lines, exit 1; a non-bundle `error: '<f>' does not look like a v2 or v3 bundle file`, `could not open`, `unrecognized header: … (n)`, `unknown capability '…'`. `list-heads <file> [<ref>...]` and `unbundle <file> [<ref>...]` (stores the objects, prints the matching refs; refs match by full name) as git.
- `fetch <bundle> [<refspec>...]` and a configured remote whose url is a bundle: refs are read from the header (in git's reversed order, which shows in the summary), prerequisites are checked first (same error block, exit 1), the pack is stored, `fetch` without a refspec on a bundle with no `HEAD` is `fatal: couldn't find remote ref HEAD` (now for any ad-hoc url without `HEAD`).
Not done / refused: `--filter`, `bundle-uri` and any v3 capability other than `object-format=sha1` (error as git), SHA-256 bundles, ranges with `...`, rev-list options other than `--all`/`--not` (refused as not implemented), `clone <bundle>` and `ls-remote <bundle>`, `remote.<n>.tagopt` (so the scenario passes `--no-tags`); git follows tags after a fetch with an explicit refspec where rivet does not (pre-existing, avoided with `--no-tags`).
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 202 PASS / `failures: 0`.

### Phase 3.16 — the four accepted divergences from 3.4 are closed (scenarios `s_reset_current_branch`, `s_cat_file_index`, `s_reflog_index_edges`)
Each reproduced against `/usr/bin/git` before the fix (captured from the first scenario runs):
- `switch -C <current>` / `checkout -B <current>` printed `Switched to and reset branch` (or, with no start point, a clap error for `-B`): git prints `Reset branch '<b>'` when the named branch is the one already checked out. `-B <b>` no longer requires a start point (`HEAD` by default). The `HEAD` log also gets git's `branch: Reset to <start>` line ahead of the `checkout: moving from <b> to <b>` line when the reset branch is the current one, and a reset to the commit the branch already has leaves the branch's own log alone (both found by the scenario's reflog dump).
- `cat-file` on `:<path>`, `:<n>:<path>` and `:./<path>` failed with `empty revision`; it now reads the index (stage 0 by default, cwd-relative with `./`), with git's diagnoses: `path '<p>' is in the index, but not at stage <n>` + `hint: Did you mean ':<m>:<p>'?`, `exists on disk, but not in the index`, `does not exist (neither on disk nor in the index)` — also under `-e`, as git does. `:/<text>` and a lone `:` still go through the old path.
- `<ref>@{N}`: past the end `log for '<ref>' only has N entries` now also comes out of `log`/`show` (they said `ambiguous argument`); a reflog file that exists but is empty gives `@{0}` = the current value and `log for <full ref name> is empty` for anything older; `HEAD` with no log file of its own reads the log of the branch it names (git does; its error then names `refs/heads/<b>`).
Not done: `rev-parse`/`show` of `:<path>` (only `cat-file` learned the index forms), `cat-file <type> :<path>` (the positional type form is not in the frozen grammar), git's symmetrical behaviour for a ref with no log other than `HEAD`.
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 205 PASS / `failures: 0`.

### Phase 3.17 — `archive --remote` (local), `--exec`, `--worktree-attributes` (landed, scenarios `s_archive_remote`, `s_archive_worktree_attributes`)
Base `ec3bc3c`: the three options were refused by name.
- `--worktree-attributes` (new `ArchiveOptions::worktree_attributes`, documented): `.gitattributes` is read from the work tree directory by directory, and the tree's own files are not consulted at all (a directory whose work-tree copy is missing has no attributes — checked: removing the file in the work tree stops `export-ignore` although the tree and index still have it). Ignored for a bare repository, where the tree is used.
- `--remote=<repo>` for a local path or `file://` url (also `--remote <repo>`): there is no daemon; the repository is opened in place and archived with its own refs and attributes, which is what `git upload-archive` does for a local path. Compared with git: only ref names are accepted as the tree-ish (`HEAD`, branches, tags, optionally `:<path>`); anything else, an unmatched pathspec, a path that is not a repository (a subdirectory of one included) is answered the way the server's death reaches the client: `remote: fatal: …` (padded with eight spaces as git's sideband does) + `remote: git upload-archive: archiver died with error` + `fatal: sent error to the client: …`, exit 1 (git interleaves the last two nondeterministically, so the scenario compares stdout and exit status for those). The `-o` file is created before the request, empty after a failure, as in git. `--list` works. `--exec=git-upload-archive` is accepted; any other `--exec` that does not exist reproduces git's `sh` error and `fatal: the remote end hung up unexpectedly` (128); an existing one, a network url (`ssh://`, `host:path`, `http://`) and `--remote` with `--add-file`/`--worktree-attributes` are refused as not implemented.
- Found with the scenario and fixed in the core: an `export-ignore` pattern with a trailing slash (`docs/ export-ignore`) was skipped as "no directory-only form"; git matches the directory itself, which drops everything below it from an archive. Covered for plain (non-remote, non-worktree) archives too.
Not done: `--add-virtual-file`; `--remote` usage errors forwarded as `remote: usage: …` blocks (rivet prints its local usage); `export-subst` as before.
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 207 PASS / `failures: 0`.

### Phase 3.18 — `describe --contains` (landed, scenario `s_describe_contains`; `s_submodule_status` extended)
Base `568c5b2`: `--contains` was not in the grammar, and `submodule status` refused (with an error) whenever a later tag existed. `describe --contains [--all] [--always] <commit>-ish` follows `name-rev --tags --no-undefined`, compared with `/usr/bin/git` 2.56 on a linear history with lightweight and annotated tags, several tags on one commit, a nearer tag, and a merge:
- Every tag (with `--all` every ref) is walked from its tip, tags before other refs and older tagger dates first (a lightweight tag counts with its commit date); a commit keeps the first name that is better by: tag over non-tag, then fewer steps, then older tag date. A first-parent step is `~N`, a step through the Nth parent of a merge costs 65535 and is written `^N` (`mergetag^2~1`), an annotated tag's own commit is `tag^0`. `--all` names branches without `refs/heads/` and the other refs without `refs/` (`main~2`, `tags/v1~1`).
- No tag contains the commit: `fatal: cannot describe '<full id>'` (128), or the abbreviated id with `--always`; an unresolvable argument is `Could not get sha1 for <arg>. Skipping.` on stderr with exit 0 (name-rev's behaviour). `--long`, `--exact-match` and `--abbrev` are accepted and ignored, as git does.
- `submodule status` now uses it for its third step (`describe`, `--tags`, `--contains`, `--all --always`) instead of refusing; covered with a submodule detached before a later annotated tag (`(later~2)`).
Not done: `--match`/`--exclude` (not in the grammar), several commits per call, and the commit-date cutoff `name-rev` uses as an optimisation (it never changes the name of the target).
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 208 PASS / `failures: 0`.

### Phase 3.19 — `submodule set-branch` and `set-url`; status no longer lists what is inside a gitlink (landed, scenario `s_submodule_set`)
Base `c7f7ae0`: both subcommands refused by name.
- `set-branch (-b|--branch <b> | -d|--default) [-q] [--] <path>` edits `.gitmodules` as text (comments and layout of the rest are kept; the key goes after the section's last entry, `-d` removes it and exits 1 when it was not there). Errors as git: no flag `--branch or --default required`, both `options '--branch' and '--default' cannot be used together` (128), a path with no mapping `no submodule mapping found in .gitmodules for path '<p>'` (128), wrong argument count the two-line `set-branch` usage (129). The file is left modified, not staged.
- `set-url [-q] [--] <path> <newurl>` rewrites `url` in `.gitmodules` the same way (creating the section, or the file, when it is missing) and then runs `sync` for that path, which prints `Synchronizing submodule url for '<p>'` and updates `.git/config` and the submodule's remote. When the edited file no longer maps the path (it was just created with the url alone) git still syncs with the name and url it holds; so does this.
- Found with the scenario and fixed in the core: `status` listed the nested repositories of a checked-out submodule (`?? vendor/lib/in/`) as untracked content of the superproject. Anything below a tracked gitlink is now left out of the untracked and ignored lists.
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 209 PASS / `failures: 0`.

### Phase 3.20 — `submodule absorbgitdirs` (landed, scenario `s_submodule_absorb`)
Base `2c2b931`: refused by name. `submodule absorbgitdirs [--] [<path>...]`, compared with `/usr/bin/git` 2.56: for each gitlink (cwd-relative pathspecs) whose work tree holds a real `.git` directory, that directory is moved to `.git/modules/<name>` (`Migrating git directory of '<p>' from` / `'<abs>/.git' to` / `'<abs>/.git/modules/<name>'` on stderr), `<p>/.git` becomes `gitdir: <relative path>` and the module config gets the relative `core.worktree`; submodules that already use a `.git` file are left alone, and the walk continues into the submodules of each one (display paths `vendor/lib/in`). Errors: unmatched pathspec (1), a gitlink without a name in `.gitmodules` `fatal: could not lookup name for submodule '<p>'` (128, earlier ones stay migrated), unknown option `error: unknown option \`x'` / short `error: unknown switch \`q'` + usage (129). A target module directory that already exists is refused as not implemented (git has its own rules for reusing it).
Also fixed on the way (found by this scenario): `submodule status` printed a plain state for a submodule that is checked out but not registered (`submodule.<name>.url` missing); git prints `-<id> <path>` for anything that is not active.
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 210 PASS / `failures: 0` (confirmed below).

### Phase 3.21 — `submodule summary`; `submodule <unknown>` prints git's usage; `GAPS.md` (landed, scenario `s_submodule_summary`)
Base `b217163`. The block hit its session limit mid-unit: the handler and scenario were written and the scenario passed, but `fmt`, `clippy` and the commit were left undone; finished afterwards.
- `submodule summary [--cached|--files] [-n|--summary-limit <n>] [<commit>] [--] [<path>...]` compares gitlinks three ways — work tree against `HEAD` by default, `--cached` index against `HEAD`, `--files` work tree against the index — and prints `* <path> <old>...<new>` with `(N):` and `  > <subject>` lines, `(submodule)->(blob)` forms for a gitlink replaced by a file, and the `Warn: <p> doesn't contain commit <id>` variants when one side's commit is not in the submodule. Ids are abbreviated from the submodule when it holds the commit, else from the superproject; the walk is first-parent. Non-gitlink paths are ignored; `-n 0` prints nothing; `--cached --files` is `fatal: … cannot be used together` (128); a bad `--summary-limit` is the option error (129); an unknown option or a missing argument prints the usage (1).
- Fixed while landing: an unknown subcommand word printed a stale `submodule: '<x>' is not implemented (all but 'summary' are)`; it now prints the same usage block as git and exits 1, byte-for-byte identical for `submodule frobnicate`.
- Added `GAPS.md` — missing subsystems (hooks, credential helpers, sparse checkout/partial clone, `rebase --interactive`, `symbolic-ref`, bundle-uri, advanced submodule update modes), partial commands, and deliberate divergences — and linked it from the README's Status section, which pointed at `.claude/PRIORITIES.md`, a path excluded from git and therefore dead on GitHub. The `submodule` module doc no longer says summary is refused.
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 211 PASS / `failures: 0`.

## Block 4 (hooks / symbolic-ref)

Base `bf5d17d`. Gate at the base: fmt, clippy, doc clean; 229 passed / 0 failed; `scenarios.sh` 211 PASS / `failures: 0` on the second run. The first run had one failure, `s_notes_fanout_deep` (`notes add` timed out with exit 124 inside the harness while the machine was busy); it passed in 4 isolated runs and in the full rerun, so it is a load-dependent timeout, not a regression.

### Phase 4.1 — `symbolic-ref` (landed, scenario `s_symbolic_ref`)
Base: the subcommand did not exist. Compared with `/usr/bin/git` 2.56 in scratch repositories:
- Read: `symbolic-ref [--short] [-q] [--[no-]recurse] <name>`; a ref that is direct, missing or detached is `fatal: ref <name> is not a symbolic ref` (128), `-q` is silent with exit 1; a malformed name is `fatal: No such ref: <name>`; `--short` uses the same unambiguous shortening as `for-each-ref :short`; a dangling target still reads.
- Write: `symbolic-ref [-m <reason>] <name> <ref>`: `HEAD` outside `refs/` is `Refusing to point HEAD outside of refs/`, an invalid target `Refusing to set '<n>' to invalid ref '<t>'`. A write logs to the reflog of the name (when the ref keeps one) only if the new target resolves, with an empty message unless `-m` is given; that empty message is written with no tab.
- Delete: `-d/--delete <name>`: `Cannot delete <name>, not a symbolic ref` and `deleting 'HEAD' is not allowed` (128); wrong argument counts print git's usage block, exit 129.
- Found by the scenario and fixed: `ReflogEntry::to_bytes` wrote a tab after the signature for an empty message (its doc claimed git does); git writes none. The `worktree add` workaround that cut the tab off afterwards was removed (the worktree scenarios caught the double fix). `branch` listed a symbolic branch as a plain one and failed on a dangling one; it now prints `alias -> target` and skips the dangling ref, as git does (scenario `s_symbolic_ref`).
Not done: a one-level lowercase target (`symbolic-ref FOO foo`) is refused, and git's writing of a ref with a malformed name (`refs/heads/a..b`) is not reproduced; both are in `GAPS.md`.
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 212 PASS / `failures: 0`.

### Phase 4.2 — hooks: runner, `commit`, `merge`, `switch`/`checkout`, `core.hooksPath` (landed, scenarios `s_hooks_commit`, `s_hooks_merge`, `s_hooks_checkout`, `s_hooks_hookspath`)
Base `7d37e39`: no hook was ever run. Every behaviour below was read off `/usr/bin/git` 2.56 first, then pinned by a scenario whose hooks append argv, stdin, cwd and the `GIT_*` variables to `hooks.log` inside the work tree (compared by the harness's work-tree dump) and echo to stdout and stderr (compared in the transcript).
- New `rivet_core::hooks::Hook` (documented): looks the hook up in `core.hooksPath` (relative → work tree root, also from a subdirectory; `~` expanded; empty → default) or `<common dir>/hooks`; a file without an executable bit is skipped with git's `hint: The '<path>' hook was ignored because it's not set as executable.` once per hook and process, silenced by `advice.ignoredHook=false`; a directory is run and fails like git (`fatal: cannot exec '<path>': Permission denied`, status 128 → the command ends 1); a script without `#!` is retried through `sh`; cwd is the work tree root; stdin empty (or the given bytes); **stdout is redirected to stderr**; `GIT_PREFIX` always (`sub/`), `GIT_INDEX_FILE=.git/index` / `GIT_EDITOR=:` / `GIT_REFLOG_ACTION` where git sets them.
- `commit`: `pre-commit` (before the nothing-to-commit check, as git), `prepare-commit-msg .git/COMMIT_EDITMSG message`, `commit-msg .git/COMMIT_EDITMSG`, `post-commit`, then `post-rewrite amend` (stdin `<old> <new>`) for `--amend`. New `-n/--no-verify` skips `pre-commit` and `commit-msg` only. A failing hook ends the commit with exit 1 and no message of its own; `post-commit`/`post-rewrite` failures are ignored; a message the hooks emptied is `Aborting commit due to empty commit message.`; `COMMIT_EDITMSG` is now written, as git does.
- `merge` (shared with `pull`): `pre-merge-commit`, `prepare-commit-msg .git/MERGE_MSG merge`, `commit-msg` for a clean merge commit (`MERGE_MSG` without a trailing newline, as git writes it), and `post-merge 0|1` after a merge, fast-forward or squash; a failing commit hook leaves the merge staged and in progress, `Not committing merge; use 'git commit' to complete the merge.`, exit 1. Found by the scenario: `--no-commit`'s "Automatic merge went well…" belongs on stderr.
- `switch`/`checkout`/`checkout -b`/`-B`/`--orphan`/paths: `post-checkout <old> <new> <1|0>` (null id for an unborn side), also for "Already on", after git's message; a failure ends the command with 1 for branches and with the hook's own status for `checkout -- <path>`, as git does.
Not done (see `GAPS.md`): `merge --continue`, `cherry-pick`, `revert`, `am`, `stash`, `rebase`, `push`, `clone`/`worktree add` hooks, `reference-transaction` and the rest of the hook list.
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored (the first run had one failure, `gc_cruft_packs_the_young_commit…`, whose helper reads a fixed `/tmp/blob.tmp` and hit `short read while indexing`; it passed in three isolated runs and in the full rerun, so it is a shared-/tmp race in the test, not this change); `scenarios.sh` 216 PASS / `failures: 0`.

### Phase 4.3 — hooks: `rebase` and `push` (landed, scenarios `s_hooks_rebase`, `s_hooks_push`)
Base `7a6f9cc`. Read off `/usr/bin/git` 2.56, then pinned by scenarios (they also dump `hooks.log` into the transcript, so a difference reads directly in `transcript.diff`):
- `rebase`: `pre-rebase <upstream> [<branch>]` runs only when there is something to do (git prints "Current branch … is up to date" without it) and not under the new `--no-verify`; a refusal is `error: The pre-rebase hook refused to rebase.`, exit 1 (a stashed autostash is put back). One `post-checkout <old head> <onto> 1` for the move onto the new base; `post-commit` for every commit the rebase writes (`GIT_REFLOG_ACTION=rebase (continue)` for the commit made by `--continue`); at the end `post-rewrite rebase` with one `<old> <new>` line per rewritten commit (kept in `rebase-merge/rewritten-list`, so it survives a stop and `--continue`). Under `pull --rebase` every one of these gets `GIT_REFLOG_ACTION=pull --rebase`, and `pre-rebase` is handed the fetched tip, as git does.
- `push`: `pre-push <remote as typed> <url as configured>` with stdin `<local ref> <local sha> <remote ref> <remote sha>` for every ref that will actually be sent (`(delete)` and a zero sha for deletions, the full ref name for a branch or tag argument, `HEAD` as typed; up-to-date refs and refused non-fast-forwards are left out, but the hook still runs with an empty stdin), after the remote has been asked what it has and before anything is sent; `--dry-run` runs it, `--no-verify` (accepted before, ignored) skips it; a refusal prints `error: failed to push some refs to '<url>'` and exits 1 with nothing sent. `pre-push` is the one hook whose stdout stays on stdout (`Hook::keep_stdout`). `PushRequest::no_verify` and `PushReport::hook_refused` are new fields of the core API (documented).
Not done: see `GAPS.md` (other commit-making commands, `clone`/`worktree add`, `hook run`); `push <missing-ref>` still exits 128 where git exits 1 (found by the scenario, outside hooks, left out of it).
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 218 PASS / `failures: 0`.

### Phase 4.4 — `hook run` (landed, scenario `s_hooks_run`)
Base `99670cb`: no `hook` command. `hook run [--allow-unknown-hook-name] [--ignore-missing] [--to-stdin=<path>] [-j <n>] <hook-name> [-- <args>]` against git 2.56: runs the hook through the same runner (so `core.hooksPath`, the hint for a non-executable file, `GIT_PREFIX`, stdout-to-stderr), exits with the hook's status; a missing hook is `error: cannot find a hook named <n>` (1) unless `--ignore-missing`; a name outside `rivet_core::hooks::NATIVE_HOOKS` is `error: unknown hook event '<n>';` + the `--allow-unknown-hook-name` line (1); no name prints git's usage (129), no subcommand `error: need a subcommand` + usage (129); an unreadable `--to-stdin` file is `fatal: could not open '<f>' for reading: …` (128). Not done: `hook list`, parallel `-j` (accepted, ignored), git's own option-error text for unknown flags (clap's is shown).
Gate after: fmt, clippy, doc clean; 229 passed / 0 failed / 0 ignored; `scenarios.sh` 219 PASS / `failures: 0`.
