# Known gaps

rivet is a Git-compatible implementation, not a wrapper: every command below is compared
against the real `git` binary by the differential suite in `tools/difftest/scenarios.sh`
(currently 465 scenarios). This file lists what does **not** work yet, so that nobody has to
discover it by running a command that fails.

Last checked 2026-11-10, gate: 355 tests / 0 failed, 465 scenarios / 0 failures, `fmt`,
`clippy -D warnings` and `doc` clean.

Three different things get called "not working". They are separated here on purpose, because
only the first two are defects.

## 1. Not implemented at all

The command or subsystem is absent; it either fails at the argument parser or refuses by name.

* **Split index** — read (`link` extension merged with `sharedindex.<hash>`, deletions, replacements, additions) and written (`core.splitIndex`, `splitIndex.maxPercentChange`, `update-index --[no-]split-index`; `s_split_index`, which has git read what rivet wrote and the reverse). A new shared index is written when entries the shared one lacks exceed `splitIndex.maxPercentChange` (0 always, 100 never), and then the other `sharedindex.*` files not newer than `splitIndex.sharedIndexExpire` (`never`, `now`, `N.units.ago`; default 2 weeks) are deleted; reading or writing a split index touches the shared index it stands on, so one in use does not age (`s_split_index_expire`). An unusable `sharedIndexExpire` (`false`, `all`, text git cannot read as a date) is fatal as in git once a write leaves a second shared index: `error: Invalid splitindex.sharedindexexpire: '<v>'`, `fatal: bad config variable … at line N`, exit 128, the new shared index kept and the index file not replaced (`s_split_index_expire_invalid`); the file and line come from the repository's own config, a value set in a global config prints git's command-line wording instead of its file. Not done: `gc`/`update-index --no-split-index` leave shared indexes alone as in git. `config` edits keep the file as it was apart from the lines they change (comments, header spelling, `s_config_preserve`). A continued value (`k = one\` + next line) joins its lines as git does (`s_config_continued`). An unquoted `#` or `;` ends a value wherever it stands, blanks before it dropped, quoted ones kept (`k = x#y` reads `x`, `u = https://h/#frag` reads `https://h/`; `s_config_comment_chars`). An emptied section goes with the empty headers of the same section around it, but not past a comment, another entry or a header of another section (`s_config_emptied`). Index v4 is read and written, and `index.version` (2 to 4) / `feature.manyFiles` pick the version of a new index (`s_index_version`); a non-numeric `index.version` is fatal on an index write as in git (`fatal: bad numeric config value '<v>' for 'index.version' in file .git/config: invalid unit|out of range`, `s_index_version_bad`); a number outside 2 to 4 is ignored. Not done: git also dies in `status` (which refreshes the index) and prints a warning for some out-of-range values; rivet's `status` does not write the index here, and `-c` is not a rivet option.
* **Partial clone** — promisor/partial clones are not implemented: no filtered transfer, no lazy
  fetch of missing objects. `clone --filter=blob:none|tree:<n>|blob:limit=<n>[kmg]` is accepted only
  where git itself ignores the filter (a local path, or a `file://` URL whose repository does not set
  `uploadpack.allowFilter`): git's warning is printed, the full clone is made and
  `remote.<name>.promisor` / `partialclonefilter` / `core.repositoryformatversion=1` are written as git
  does (`s_clone_filter`). Refused by name (`clone_filter_refusals.rs`): a source that honours filters, a
  bundle or network source, and the specs `sparse:`, `object:type=` and `combine:`. The source's
  `uploadpack.allowFilter` is read from its own repository config only, not from global config.
* **Sparse checkout** — `sparse-checkout init`/`set`/`add`/`list`/`reapply`/`disable`/`check-rules` exist
  (cone mode and `--no-cone`, `--stdin`, `--skip-checks`; `check-rules` with `-z`, `--cone`/`--no-cone`,
  `--rules-file`, C-quoted input and output; `clean` with `-n`/`-f`/`-v`, `s_sparse_clean`). The sparse index works the way commands see it: a
  file with the `sdir` extension is read by expanding each directory entry to its tree's files
  (`skip-worktree`), and with `index.sparse` in a cone-mode checkout the index is written back sparse
  (directories wholly outside the cone as one `040000` entry); `--[no-]sparse-index` set
  `index.sparse` (`--sparse-index` alone implies cone mode, as in git) (`s_sparse_index`). Not done: a
  sparse index is expanded in memory on every command, never operated on sparsely, so there is no
  speed-up and git's `advice.sparseIndexExpanded` hint is never printed; the raw `Index::load` and
  `Index::parse` still refuse `sdir` by name (only `Repository` expands); a directory holding an unmerged
  entry stays expanded in the file.
  In cone mode `set`/`add` take command-line paths relative to the current directory (`..` resolved,
  `outside repository` refused) and `--stdin` lines relative to the root; `--no-cone` from a
  subdirectory is refused as in git. `ls-files --sparse` is accepted (the index is never sparse, so it
  changes nothing) and long `status` prints the "You are in a sparse checkout with N% …" line.
  A cone file that is not in the shape git writes is read as free-form patterns, with git's two
  warnings printed once (git prints them twice); `check-rules --rules-file` instead ignores the lines
  of a cone file it does not understand, as git does, and prints no warning.
  Honoured by `switch`/`checkout`, `reset --hard`, `merge`, `cherry-pick`, `revert`, `rebase`, `stash`,
  `add`/`rm`/`mv` (git's refusal for names that only match paths outside the definition, `--sparse` for
  `add`), `restore`/`checkout -- <path>`, `diff`, `status`, `commit -a`, `add -u`, `ls-files -t`
  and `worktree add` (the new work tree inherits the definition). A merge whose conflict lands outside
  the definition matches git, including `merge --abort` (`s_sparse_leftovers`). Not covered, so they may
  ignore the definition: `update-index` details, `am`, `bisect`, and `git sparse-checkout set`
  leaving a directory that holds untracked files (the warning is printed, git also tries to remove it).
  `clean -v` lists the files of one
  directory sorted (git uses readdir order), and a nested repository's `.git` is walked like any directory.
  `check-rules` run with a pattern file but `core.sparseCheckout` off, and a path starting with `/` in
  non-cone mode, are not compared.
* **`rebase --interactive`** — `pick`/`reword`/`edit`/`squash`/`fixup`/`drop`/`noop`, the editable todo list,
  `--continue`/`--skip`/`--abort` work (`s_rebase_i`, `s_rebase_i_stops`, `s_rebase_i_conflict`).
  `--root` works, plain (`s_rebase_root`) and with `-i` (`s_rebase_root_i`; the virtual empty base is recorded in `squash-onto`, and the first commit made on it is a root commit again); too many positionals, with or without `--root`, get git's full usage text and exit 129 (`s_rebase_positional_usage`). Refused by name, before anything is touched: `--rebase-merges`, (`--exec`/`-x` works with `-i` and without it, which runs an unedited todo list, `s_rebase_exec`, `s_rebase_exec_plain`; `--autosquash` works without `-i` as well, `s_rebase_autosquash_plain`; so does an `exec` todo line; `break` works too, `s_rebase_break`; so do `fixup -C`/`-c`, `s_rebase_fixup_c`); `--keep-base` works (`s_rebase_keep_base`),
  signing, `--edit-todo` outside an interactive rebase (rivet's plain rebase has no todo to edit),
  `--autosquash` over a `fixup!` that names
  another fixup commit by id (`s_rebase_autosquash` covers the rest, and `s_rebase_autosquash_amend` the `amend!` → `fixup -C` form, plus `--no-autosquash` and
  `rebase.autoSquash`), and the todo command `merge` (`label` and `reset` work,
  `s_rebase_label_reset`; `reset` takes a label, a ref or a commit id, and the `-C` form is not parsed;
  `update-ref <ref>` works, `s_rebase_update_ref`, but a ref that moved since the rebase started, or that is
  checked out in another work tree, is not compared, and `--update-refs` / `rebase.updateRefs` generate the lines (`s_rebase_update_refs`: reverse name order within a commit, a branch checked out in another work tree as a comment, generated before `--autosquash` reorders); the state files `git-rebase-todo`/`done` keep the blank line after each generated `update-ref` line and `git-rebase-todo.backup` is the list as the editor was given it, full ids and footer, all as git writes them, `s_rebase_update_refs`; blank lines a user adds in the editor are not kept, only those after `update-ref`). A rebase stopped inside a `fixup -C`/`-c` chain and resumed by the other implementation
  is not compared (git leaves no `message-fixup` there; rivet writes one). An empty message from the editor aborts the whole rebase back to the
  original tip (git stops with the commit pending). The editor sees the message plus git's `#` footer, not
  the `git status` template. `post-rewrite` lines for a squash chain are not compared with git.
* **`bundle-uri`** (beyond `clone --bundle-uri=<file>`) and SHA-256 bundles. Creating, listing, verifying and unbundling bundles works;
  fetching, cloning and `ls-remote` *from* a bundle file work (`s_bundle_clone`, `s_ls_remote`). `clone --bundle-uri` takes a file path
  or `file://` URL only (refs land in `refs/bundles/*`, `s_clone_bundle_uri`); other schemes warn and fall back to a plain clone, as an
  unreachable URI does in git. Bundle lists (`bundle.*` config), `fetch.bundleURI` and the `bundle-uri` protocol command are not
  implemented; a file that is not a bundle gets git's two warnings but not the `bad config line` error naming a random temp file.
  `ls-remote --sort=<key>` works for `refname` (also `lstrip`/`rstrip`/`strip`, a leading `-`, several `--sort`, `s_ls_remote_sort`);
  `version:`/`v:` over `refname` (`s_ls_remote_version_sort`) works too (`refname:short` shortens against the local refs, `s_ls_remote_sort_short`); `*` (ignore case) and the object-data keys are refused by name, and git's own
  error texts are matched for unknown keys. `--upload-pack` and `--server-option` are refused by name (Rust test
  `ls_remote_refusals`). A local path that is a subdirectory of a repository is listed as that repository (rivet's discovery walks
  upwards; git says "does not appear to be a git repository"; a path that does not exist is matched); its patterns follow `wildmatch` (`*`, `?`, `[...]` with ranges, negation and `[:class:]`, `\` escapes, `s_ls_remote_patterns`).
* **Submodule clone flags** — `update --depth` works only for a local-path source (git ignores it with
  a warning; rivet narrows the fetch refspec to the one branch, and refuses a source with several
  branches). With a `file://` or network URL git makes a shallow clone, which rivet's `clone` cannot
  write: refused by name. `--single-branch` / `--no-single-branch` work like `--depth`'s narrowing: the
  fetch refspec names the one cloned branch, and a source with several branches is refused by name (rivet's
  `clone` cannot fetch a single branch only). `--filter` is refused with "not implemented".
  `--reference` writes the module's `objects/info/alternates` (rivet still copies every object; it reads
  alternates since block 12); `--dissociate` leaves none. `-j`/`--jobs` is validated and accepted but runs sequentially.
  `checkout`, `merge` and `rebase` (`--merge`, `--rebase`, `submodule.<name>.update`) work. `update --remote` works
  (branch from `.gitmodules`, remote `HEAD` by default); `submodule.<name>.branch = .` is refused.
* **`status` of a submodule** — a submodule whose `HEAD` differs from the index, or whose tree is dirty, is reported
  (` M path`, `modified:   path (new commits, modified content, untracked content)`, short ` m` / ` ?`, git's
  "commit or discard" hint; `s_status_moved_submodule`, `s_status_dirty_submodule`). A submodule nested in a changed submodule counts as `modified content`; `--ignore-submodules[=all|dirty|untracked|none]`,
  `submodule.<name>.ignore` (config over `.gitmodules`), `diff.ignoreSubmodules`, `-uno` (hides a submodule's untracked content) and
  `status.submoduleSummary` (true or a limit: the `Submodule changes to be committed:` / `Submodules changed but not updated:` blocks)
  are compared (`s_submodule_status_dirty`). Not compared: the summary blocks in `commit`'s editor template and `--short`/`--porcelain` (git prints none there). `diff`/`commit -a`/`add -u`
  still skip gitlinks. `status` takes several literal pathspecs and reports their union (`s_status_multi_pathspec`); pathspec magic works there too (`:(exclude)`/`:!`/`:^`, `:(top)`/`:/`, `:(glob)`, `:(literal)`, `:(icase)`, bare wildcards, `..` in patterns, git's errors; `s_status_pathspec_magic`); `:(attr:...)` is refused by name. A wholly untracked directory named by a literal pathspec is reported as `dir/` (`s_status_untracked_dir_pathspec`). A pathspec outside the tree gets git's `outside repository at` error, plain or magic (`s_status_outside_pathspec`).

## 2. Implemented, but partial

The command works for the common case; the listed options or edge cases are missing or refused.

* **`log`** — takes any number of revisions and ranges (`s_log_multi`); `--color` paints `-p` and
  `diff --color` the patch with git's default palette (`s_diff_color`), `--name-*` are plain as in git.
  `--stat` is painted too (`s_log_stat_color`; `log`, `show`, `diff`). Refused by name (Rust tests in
  `tests/diff_color.rs`): any `color.diff` / `color.diff.<slot>` setting, a space before a tab in an added line (git highlights it), colour
  for the conflicted-path output of `diff`; `log --first-parent` diffs a merge against its first parent and an
  empty `--format=` works (`s_log_first_parent_merge_diff`); the last of `--oneline` / `--pretty` / `--format`
  wins and `--oneline` abbreviates the `commit` line of a later layout (`s_log_layout_order`, `s_show_oneline`; read
  from the raw command line); `--graph` over several root commits exits 128 by design; `--graph` with `--date-order` or
  `--first-parent` orders differently from git.
* **`rev-list`** — `--cherry-pick` and `--cherry-mark` work (`s_rev_list_cherry`; `--cherry`, the synonym for `--right-only --cherry-mark --no-merges`, `s_rev_list_cherry_synonym`). `log --cherry-pick`, `--left-only` and `--right-only` filter as `rev-list` does (`s_log_cherry_pick`); `log --left-right` and `%m` work (`s_log_left_right`), and so does `log --boundary` (`-` or `o` in a graph, after the commits, `s_log_boundary`). `--children` works (`s_rev_list_children`). `--left-right`, `--left-only`
  and `--right-only` work (`s_rev_list_left_right`), and so does `--ancestry-path` without a
  commit argument (`s_rev_list_ancestry_path`); `--ancestry-path=<commit>` keeps the commit, its
  descendants and its ancestors (`s_rev_list_ancestry_path_commit`), flagging the ancestors in walk order
  as git does so skewed dates drop the same ones (`s_rev_list_ancestry_path_skew`).
* **`for-each-ref`** — `--shell`/`--perl`/`--python`/`--tcl` (and `-s`/`-p`, `--no-…`) work (`s_for_each_ref_quote`);
  `--stdin` and `--start-after` / `--no-start-after` work (`s_for_each_ref_stdin_start`);
  `--include-root-refs` / `--no-include-root-refs` work (`s_for_each_ref_root_refs`: `HEAD` and `[A-Z_]*_HEAD` files of the git
  directory, not `FETCH_HEAD`/`MERGE_HEAD`, git's "ignoring broken ref" warning, `%(symref)`, commit filters drop them); only the
  work tree's own git directory is scanned, so a linked work tree is not compared. `--sort=version:<key>`/`v:<key>` follows git's `versioncmp` (`s_for_each_ref_version_sort`); `versionsort.suffix` (and the deprecated `prereleasesuffix`) is honoured (`s_for_each_ref_version_suffix`, fuzzed against git); `ls-remote --sort=version:` honours it too (`s_ls_remote_version_suffix`; the repository's own config only, not global). Pattern and `--exclude` globs follow `wildmatch` with `WM_PATHNAME` (`s_for_each_ref_patterns`).
* **`blame`** — `-M`/`-C` work over a file with uncommitted changes (`s_blame_worktree_move`), but `--incremental` then lists the hunks of one commit in a different order from git. `--contents <file>` (`-` is standard input; the lines it changes are `External file (--contents)`, `s_blame_contents`), `-n`/`--show-number` (`s_blame_flags`), `--since`/`--after` (older commits become boundary commits; an unparsable date means now, as in git), `-S <revs-file>` (git's graft file: `<commit> <parent>...` lines, `bad graft data` errors) and `-L :<funcname>` / `-L ^:<funcname>` (`s_blame_funcname`) work. `-L :<funcname>` uses git's default function-line rule or a `diff=<driver>` with `diff.<driver>.xfuncname` / `funcname` (negated patterns included); a driver that names one of git's built-in userdiff drivers (`diff=cpp`, `java`, ...) is refused by name (`blame_funcname_refusal.rs`). `-L` with `/regex/`, `^/regex/` and `<start>,/regex/` works (`s_blame_regex_range`); the regex is a basic one from rivet's own matcher, so the text of a *compile* error can differ from glibc's for a few malformed patterns (git says `Invalid regular expression` for `[`). Not done: `--reverse`.
* **`grep`** — `-o`, `--heading`, `-z`/`--null`, `--break` and `--untracked` work (`s_grep_output_modes`, `s_grep_null_break`, `s_grep_untracked`; `--untracked` searches the non-ignored untracked files merged in path order, refuses `--cached`, and reads a revision as a path);
  `--break` over a binary file that matches is refused by name (git drops or keeps the notice depending on
  its worker threads; Rust test `grep_break_refuses_binary`). `--threads=<n>` / `--no-threads` are validated like git's and
  otherwise ignored (`s_grep_threads`). `--no-index` searches the current directory in or out of a repository, `.git` skipped, no ignore file consulted unless `--exclude-standard` asks for one (`s_grep_no_index`). `--[no-]exclude-standard` works with `--untracked` (on by default) and `--no-index` (off by default), the last one wins, and a tracked search refuses both (`s_grep_exclude_standard`); `--no-index --exclude-standard` reads `.gitignore` files and `info/exclude`, not `core.excludesFile`. No `-P`.
* **`apply`** — `GIT binary patch` (`diff --binary`, literal and delta blocks) is decoded, with `-R`
  taking the reverse block. `am --3way` still refuses binary patches. `-C<n>` reduces
  context like git (with its `Context reduced to (l/t) to apply fragment at N` note), symlinks can be created,
  deleted and retargeted (`wrong type` as git); `--reject` with `--3way` is git's `options '--reject' and '--3way' cannot be used together` (`s_apply_three_way`). `--whitespace=<action>` / `apply.whitespace` check and repair the added lines (`s_apply_whitespace`); `--no-add` drops the additions (`s_apply_no_add`); `--ignore-whitespace`/`--ignore-space-change` and `--inaccurate-eof` are unknown, and `--whitespace=fix` still needs the context lines to match exactly where git forgives whitespace-only differences.
* **`format-patch` / `am`** — `--cover-letter` (`0000-cover-letter.patch`: shortlog wrapped at 72 columns,
  the range's diffstat and summary, numbering `0/N`), `--thread[=shallow|deep]`, `--no-thread` and `--in-reply-to`
  (`Message-ID`, `In-Reply-To`, `References`; a deep thread lists every earlier message), and RFC 2047 encoding of
  non-ASCII names and subjects (folded at 76 columns, the address moved to its own line past 78, the MIME headers
  added for a non-ASCII subject or message, ASCII names with a special quoted) are compared with git
  (`s_format_patch_cover`; the message ids carry the clock, so the scenario normalises them). `--range-diff` and scissors are refused. `format.coverLetter` (`true`, `false`, `auto`: only for more than one patch) and `format.thread` (`true`, `deep`, `shallow`) set the defaults the command line overrides, a bad boolean is `fatal: bad boolean config value '<v>' for 'format.coverletter'` (`s_format_patch_config`). An identity's name and email lose leading and trailing crud as git's ident cleanup does, and a run of whitespace in a reflog message is one space (`s_ident_crud_reflog`). `-v <n>` / `--reroll-count=<n>` (any text; `[PATCH v<n>]` after the prefix, a sanitised `v<n>-` in front of every file name, the cover letter included; the last one wins, `--no-reroll-count` drops it; `s_format_patch_reroll`). `am --3way`/`-3`, `--no-3way`, `am.threeWay` and `--[no-]rerere-autoupdate` work (`s_am_3way`, `s_am_3way_rerere`); `am --show-current-patch[=raw|diff]` prints the stored message (with its `From ` line) or patch, with git's refusals for no operation in progress, a bad value and a clash with `--continue`/`--skip`/`--abort` (exit 129; `s_am_show_current_patch`).
* **`stash`** — `create`, `store` and `save` exist (`s_stash_stack`), as does `branch`
  (`s_stash_branch`), and `save`/`store` error text matches (`s_stash_errors`). `push`/`save` `-S`/`--staged` (the stash is the index; the staged changes leave the work tree through a
  reverse `apply` with one line of context and are reset in the index, `-k` keeps the index, a pathspec
  only selects which paths are reset in the index, `Can't use --staged and --include-untracked or --all`,
  `No staged changes`; `s_stash_staged`, including `apply`, `apply --index` and `pop`). A staged binary file or
  submodule is refused by name. Not matched: `-p`/`--patch` (interactive hunk selection) is refused by name
  for `push` and `save` (`stash_patch_refusal.rs`); `--pathspec-from-file` and `--pathspec-file-nul` work for `push` (`s_stash_pathspec_file`; `save` and `create` have neither, as in git).
* **`gc` / `repack`** — `gc --cruft` and `repack --cruft` / `--cruft-expiration` are compared with git
  (`s_gc_cruft`, `s_gc_cruft_expire`, `s_repack_cruft`: what the cruft pack keeps, `-A`/`-k` refused with git's
  words). `repack --geometric=<n>` rolls packs up as git's split does (by object count, the loose objects joining
  the new pack, `.keep` and cruft packs left alone; `s_repack_geometric`), but compared as sets of objects: the
  order inside the rolled-up pack is rivet's own, no multi-pack-index is written, a malformed factor prints only
  the one-line error (exit 129, no usage text), and a factor outside a C `int` once `k`/`m`/`g` is applied (or too big for
  any integer) is git's `value <text> for option `geometric' not in range [-2147483648,2147483647]` (`s_repack_geometric_range`). `--max-pack-size` and `--max-cruft-size` are read and warned about as git does (`s_repack_max_pack_size`, `s_repack_max_cruft_size`) but a pack is never split at the limit. `gc --auto` counts the packs `gc.bigPackThreshold` keeps toward `gc.autoPackLimit` and has no fallback to the largest pack alone; `repack -k` over several packs lists unreachable objects in a
  different order from git.
* **`bisect`** — `start --first-parent`, path arguments to `start` (commits that leave the paths alone are never tested; a merge identical to one parent follows only that parent) and `--reset-when-found[=original|found]` on `start` and `run` are compared with git, by hand and with `run` (`s_bisect_flags`: `BISECT_FIRST_PARENT`, `BISECT_NAMES`, `BISECT_RESET_WHEN_FOUND`, `Bisecting: -1 revisions left`, `No testable commit found` ending the bisection, a silent reset to the start or to the commit found, and the `BISECT_TERMS` file git leaves after a hand-driven finish). Not matched: a path that selects only part of a merge's history is simplified like git's default only for the cases in the scenario (no `--full-history`, no parent rewriting across non-merge commits).
  `visualize`/`view` run `log` over the bad commit and `^` each good one, with git's quirk that
  `--bisect` is appended after the caller's words (`s_bisect_visualize`); a first word `log` is the `log` form and a word that names no command is git's `is not a git command` (`s_bisect_visualize`); the `gitk` form (a display
  and `gitk` on `PATH`) and a first word naming another command (git runs it with `--bisect` appended) are refused by name
  (`bisect_visualize_refusals.rs`).
* **`archive`** — `export-subst` is refused (deliberate); gzip and deflated-zip bytes are not
  byte-identical to git's (decompressed content is); `--remote` works for local repositories
  and `file://`, not over a network URL.
* **`submodule summary`** — compares gitlinks only; a submodule whose `.gitmodules` entry is
  missing is reported as git does, but submodule `HEAD` discovery outside
  `Repository::open_or_discover` (`core.worktree` variants) and the
  `submodule.<name>.active` setting is not consulted (`submodule.<name>.ignore` is, for `--for-status`, as `status` uses it).
* **`symbolic-ref`** — read, `--short`, `-q`, `--[no-]recurse`, write (with `-m`) and
  `--delete` work, one-level lowercase targets included (`s_symbolic_ref_one_level`).
* **Hooks** — `core.hooksPath`, the executable-bit rule (with git's once-per-hook hint) and the
  runner exist (`rivet_core::hooks`). Run today: `pre-commit`, `prepare-commit-msg`, `commit-msg`,
  `post-commit` and `post-rewrite amend` for `commit` (`-n/--no-verify`); `pre-merge-commit`,
  `prepare-commit-msg`, `commit-msg` and `post-merge` for `merge` and the merge inside `pull`;
  `post-checkout` for `switch` and `checkout` (branches, paths, `-b`/`-B`/`--orphan`);
  `pre-rebase`, `post-checkout`, `post-commit` and `post-rewrite rebase` for `rebase` and
  `pull --rebase` (`--no-verify`); `pre-push` for `push` (`--no-verify`); `prepare-commit-msg` and `post-commit` for `cherry-pick` and
  `revert` (and the full `commit` set for their `--continue`; a sequence that stops on a hook failure
  keeps no `.git/sequencer`); `applypatch-msg`, `pre-applypatch` and `post-applypatch` for `am` (`-n/--no-verify`;
  git's plain `am` runs no `post-rewrite` either). `merge --continue` is `commit` with the message of `MERGE_MSG` (source `merge`;
  `pre-commit`, not `pre-merge-commit`, as in git) — but the editor never starts, so `COMMIT_EDITMSG`
  lacks the `#` status template git puts there (the scenario hides those lines). `stash` runs none (as in git, which
  creates its commits without hooks; only `stash branch` runs `post-checkout`, through its checkout). `post-checkout` for `clone` (the
  status becomes the exit status) and `worktree add`; `core.hooksPath` is also read from the global and
  system config. Not run yet: `post-index-change`, and every server-side hook. `hook run` runs one hook (`--ignore-missing`, `--allow-unknown-hook-name`, `--to-stdin`; `-j` is accepted and ignored); `hook list` lists the hooks directory only (`-z`, `--show-scope`); a repository with `hook.<name>.command`/`.event` in its config is refused by name (`fatal: hooks defined in the configuration ... not supported`, exit 128), and `hook run` still ignores such hooks. Only `commit -m/-F` exists, so `prepare-commit-msg` only ever sees the `message`
  source (and `merge`). `GIT_DIR`/`GIT_WORK_TREE` are not exported to hooks (git does not either
  for the common layout; linked work trees are untested).
* **`reference-transaction`** — deliberately not implemented: git 2.56 runs it for every ref write with
  four states (`preparing`, `prepared`, `committed`, `aborted`), `ref:<target>` in the stdin for
  symbolic updates (`switch` writes `<null> ref:refs/heads/x HEAD`), a `HEAD` entry next to the branch
  entry in `preparing` for a commit, and internal transactions on pseudo-refs (`AUTO_MERGE` is
  prepared, aborted and committed again around every commit and switch; `branch -m` aborts one
  transaction and redoes it). A hook that saw only some of those events would mislead whoever relies
  on it (replication, auditing), so rivet runs none until the whole set can be matched.
* **Found by `s_hooks_clone`, not hooks** — (`worktree add <path> <name>` makes a
  local branch from a unique remote-tracking branch, `--guess-remote` does it for the directory's name,
  `-b`/`-B` from a remote-tracking start point set `branch.<new>.remote`/`merge`, and `--track`/`--no-track`/
  `branch.autoSetupMerge` are read; `s_worktree_add_branch`; a switch given `=value`, a bad short switch and bundles are git's, `s_worktree_add_switches`.) `config --get-regexp` reads its pattern as git does: lowercased, an ERE over the whole key, a bad one `error: invalid key pattern: <p>` with exit 6 (`s_config_get_regexp`). A second word is an ERE over the value (case-sensitive, a leading `!` negates it, a bad one `error: invalid pattern: <p>` exit 6, a third word `error: wrong number of arguments, should be from 1 to 2` exit 129) and `-z` prints `key\nvalue\0` (`s_config_get_regexp_value`). A bare `key` (no `=`) reads `true` but `--list`, `-z --list` and `--get-regexp` print the key alone (a value pattern sees the empty string) and untyped `--get` prints an empty line; `-z --list` separates key and value with a newline (`s_config_bare`). `--get --type=int` fails as git does, `bad numeric config value '<v>' for '<key>' in file <path>: invalid unit` (or `out of range`; a bare key is the empty string), with the path git prints (`.git/config` for the repository's file, a bare repository's relative to the current directory), and `--type=bool` reads a number as true unless it is zero (`s_config_int_error`). A bare `key` read with `--type=path` is git's `error: missing value for '<key>'` plus `fatal: bad config line N in file F`, exit 128, in `--get`, `--get-all` and `--get-regexp`, and `--get-regexp` applies the other types to a bare key too (`int` fails, the booleans print `true`; `s_config_typed_bare`). `--get-regexp` takes its pattern after `-z`/`--null`/`--local`/`--global`/`--system`/`--type=<t>` (an unknown type is still git's fatal; `s_config_get_regexp_opts`). The pattern also follows a two-word `--type <t>` (`s_config_get_regexp_type_word`). `-t <type>` and `-t<type>` are `--type` (`s_config_type_short`). `--type=bool-or-int` prints a boolean word as `true`/`false` and anything else as a C `int` with `k`/`m`/`g` (`1` stays `1`, `5g` is out of range), on read, with `--get-regexp`/`--get-all` and on write (`yes` is stored as `true`, `2k` as `2048`); `--get-regexp` applies `--type` to every type (`s_config_bool_or_int`). An unknown `--type` (including `str`, which git does not have) is git's `fatal: unrecognized --type argument, <t>`, exit 128, whatever else is on the line (`s_config_type_unknown`); `--type=bool-or-str` prints a boolean word or a number as `true`/`false` (a bare key is `true`, an empty value `false`) and any other text as written, on read, `--get-regexp`/`--get-all` and write (`s_config_bool_or_str`); `--type=expiry-date` prints `never`/`false` as 0, `now`/`all` as 18446744073709551615 and a date as its timestamp, on read, `--get-regexp`/`--get-all` and `--list` (entries that are no date are left out); a value that is no date is `error: '<v>' for '<key>' is not a valid timestamp` and a bare key `error: missing value for '<key>'`, each followed by `fatal: bad config line N in file F`, exit 128; a write stores the text as given, as git does (`s_config_expiry_date`, compared on the clock-free forms only: relative and bare-date forms use rivet's own approxidate, and git's `--list` with a bare key crashes, so it is not compared). `--type=color` prints the escape sequence of a colour (`bold red` is `ESC[1;31m`, no reset), on read, `--get-regexp`/`--get-all` and `--list` (entries that are no colour are left out); a bad colour is `error: invalid color value: <v>` (a bare key `missing value`) plus `fatal: bad config line N in file F`, exit 128, and a write checks it (`error: invalid color value: <v>`, `fatal: cannot parse color '<v>'`, exit 128) but stores the text as given (`s_config_color`; git's `--list` with a bare key crashes, so it is not compared). `--list` applies `--type` as git does: every value is converted, an entry that does not convert is left out (`--type=int` drops text, a bare key and anything past a C `int` for the boolean types), a bare key is `true` for the boolean types, and for `path` it is `error: missing value for '<key>'` on stderr with the entry left out and exit 0 (`s_config_list_type`); a number is a boolean only within a C `int`, so `--type=bool` rejects `3000000000` and `bool-or-str` keeps it as text. `--get-all` prints every value of a key in file order across the consulted files, with an optional value pattern (an ERE, `!` negates), `--type` and `-z` (`s_config_get_all`).
* **Credential helpers** — `credential.helper` (global, per URL, `!shell`, absolute path,
  `git-credential-<name>`; an empty value resets), `credential.useHttpPath`, `credential.username`,
  URL userinfo, `GIT_ASKPASS`/`core.askPass`/`SSH_ASKPASS` and a terminal prompt (`/dev/tty`,
  `GIT_TERMINAL_PROMPT=0` refuses) are consulted when an HTTP(S) remote answers `401`, with git's
  `get`/`store`/`erase` stream (capabilities and `wwwauth[]` included). Helper output `quit`,
  `password_expiry_utc` (an expired password is dropped) and `oauth_refresh_token` (handed back to
  `store`/`erase`) work (`s_credential_keys`), as do `authtype` with `credential` (an
  `Authorization: <authtype> <credential>` header, `s_credential_authtype`) and `url=` (replaces the credential;
  `s_credential_url`; `store`/`erase` still go to the helpers chosen for the original URL, git re-selects them
  for the new one). Helper output `state[]` and `continue=1` (with `capability[]=state`) come back to
  `store`/`erase` (`s_credential_state`); a rejected `continue=1` credential is not erased but sent back to the helpers (secrets dropped,
  `authtype`, `wwwauth[]` and the previous `state[]` in the `get`), with git's cap of two credential rounds
  per request; a third refusal ends in `fatal: unable to access '<url>': The requested URL returned error: 401`
  and a second round without `continue=1` is a plain rejection (erase, `Authentication failed`)
  (`s_credential_state`: `two`, `three`, `stop`, `never`). `state[]` is not shown to a later helper of the same
  round. Not implemented: `credential.<url>.*` keys other than `helper`,
  `useHttpPath` and `username`, `http.proxy` credentials, `credential.interactive`, the
  `git credential` command, and credentials for `ssh://` (the ssh program asks). Covered by
  `s_credential` against a local Basic-auth server; a `401` that first appears on a POST (discovery
  public, POSTs challenged) is retried like the discovery one (`s_credential_post`), but only fetch,
  clone and a first-POST refusal are compared for the failure text (`RPC failed; HTTP 401 curl 22 …`
  and `expected flush after ref listing`); a refused `receive-pack` POST is not compared.
* **Network protocol** — v2 capabilities other than `object-format=sha1` are not implemented;
  git follows tags after a fetch with an explicit refspec where rivet does not.
* **`update-ref --stdin`** — `symref-update`, `symref-create`, `symref-delete` and `symref-verify` (line and `-z` framing, git's validation text, the `ref`/`oid` old values, `symref-delete`/`-verify` refused in deref mode, a symref update through a symbolic ref pointing both the named ref and the end of the chain at the target), `option no-deref` for the next command only, and `-0`/`--batch-updates` (what holds is applied, the rest comes back as `rejected <ref> <new> <old> <reason>` with `refname conflict`, `incorrect old value provided`, `reference does not exist`, `reference already exists` and `expected symref but found regular ref`, checked when the batch commits) are compared with git (`s_update_ref_symref`), and so are C-quoted fields in line mode (`s_update_ref_quoted`); `delete` removes the ref while holding its lock (`RefStore::delete_holding`). Not done: the duplicate and old-value checks of a symref update through a chain are only verified for a plain symbolic ref. `for-each-ref %(symref)` follows a chain of symbolic refs to the ref it ends on (five hops, as git) and takes `refname`'s modifiers (`:short`, `:lstrip=`, `:rstrip=`; `s_for_each_ref_symref_chain`).
* **`commit-tree`** — `-S`/`--gpg-sign` is refused by name (`commit-tree: signing (-S) is not supported`, exit 128; `commit_tree_refusals.rs`).
* **`diff-tree`** — `--stdin` (one commit, `<commit> <parent>...` or `<tree> <tree>` per line; a line that names nothing is echoed; `error: Need exactly two trees, separated by a space`, `Object <id> not a tree`), `--numstat` (also `-z`), `--patch-with-raw`, `--patch-with-stat` and `--full-index`, and the combinations of `--raw`, `--numstat`, `--stat` and `-p` (blank line, NUL under `-z`, before the patch) are compared with git (`s_diff_tree_opts`). `-M[<n>]`/`--find-renames[=<n>]`, `-C[<n>]`/`--find-copies[=<n>]`, `--find-copies-harder` and `--no-renames` are compared with git for the raw, `--name-status`, `--name-only`, `-z`, `--stat`, `--numstat` and `-p` outputs (`R<nnn>`/`C<nnn>`, `copy from/to`, `similarity index`, a deleted file copied twice is renamed to the last destination; `s_diff_tree_renames`). `-c`/`--cc` is refused by name (`diff-tree: <option> is not implemented`, exit 128; `diff_tree_refusals.rs`). Not done for renames: `-B` break detection, `diff.renameLimit` beyond the fixed 1000 x 1000, a copy source filtered by a path limit under `--find-copies-harder` is only checked for the plain path-prefix case, and `diff` takes no `-C`, `log`/`show` neither `-M` nor `-C` (rename detection is the `diff.renames` default). `-p` of a symlink replaced by a regular file (or the reverse) prints git's deletion followed by an addition, in `diff`, `diff-tree`, `show` and `log -p` alike (`s_diff_mode_swap`; `FileDiff::split` is filled where the diff is built, so the raw, name-status and stat outputs keep their single `T` entry). `diff-index`, `diff-files` and `diff --raw`/`-p` do not exist as commands or options. `--first-parent` is accepted and changes nothing, as in git 2.x without `-m`.
* **`rerere`** — `status`, `remaining`, `diff`, `gc`, `clear`, `forget` and the hook that `merge` (also `--squash`, `--continue`), `cherry-pick`, `revert`, `rebase`, `commit`, `stash apply/pop`, `am --3way`/`--continue` and the autostash behind `checkout -m`, `switch -m` and `rebase --autostash` run are byte-compatible with git's `rr-cache` and `MERGE_RR` (`s_rerere`, `s_rerere_stash`, `s_am_3way_rerere`). `forget` also sees paths already staged: the resolve-undo (`REUC`) extension is written whenever a stage-0 entry or a removal replaces conflict stages, read back by `ls-files --resolve-undo`, `update-index --unresolve`, `--clear-resolve-undo` and `rerere forget` (in memory, as git does), kept by `commit`, and dropped by branch switches and merges like git (`s_resolve_undo`, `s_rerere_forget_staged`). Not done: the `conflict-marker-size` attribute and per-path `merge=` drivers are not honoured but refused by name where git would act on them (`merge`, `pull`, `cherry-pick`, `revert`, `rebase`, `stash`, `checkout -m`, `am -3`: `<path>: the \`merge=union\` attribute is not supported by rivet; refusing to merge ...`, `merge_attr_refusals.rs`); a `merge=<name>` with no `merge.<name>.driver` merges as text like git; `rerere` itself still uses marker width 7; the replays of `checkout -m` and `rebase --autostash` are silent and leave `MERGE_RR` removed / empty as git does, but that was verified for one conflicting file only.
* **`update-index`** — implemented: paths, `--add`/`--remove`/`--replace`/`--force-remove`/`--info-only`, `--refresh`/`--really-refresh` (`-q`, `--unmerged`, `--ignore-submodules`, `--ignore-missing`), `--cacheinfo` (both spellings), `--index-info`, `--stdin` with `-z`, `--chmod`, `--[no-]assume-unchanged`, `--[no-]skip-worktree`, `--clear-resolve-undo`, `--force-write-index`, `--verbose`, and the format options (`s_update_index`, `s_split_index`, `s_index_version`). `--unresolve` puts the recorded stages back (all following words are paths). `--again`/`-g` re-reads every merged entry that differs from `HEAD` (the following words are pathspecs) with the options given before it (`s_update_index_again`). Refused by name (`update-index: --<opt> is not implemented`, exit 128; `update_index_refusals.rs`): `--[no-]untracked-cache` and its test/force forms, `--fsmonitor*`, and a `--cacheinfo` of mode `040000` (git stores a sparse-directory entry). Not reproduced: git's skipping of the object write when the entry already matches the file's stat data. `add -N` writes the intent-to-add bit (v3 index, empty blob stored); `status`, `diff`, `write-tree`, `commit`, `commit -a`, `reset` and `rm --cached` treat such an entry like git (`s_add_intent_to_add`).
* **`commit-graph` and `multi-pack-index`** — read only (`rivet_core::commit_graph`, `rivet_core::midx`; rivet never writes them). The commit-graph (single file, or a split chain) answers tree, parents, commit time and generation for the history walk (`walk_oids`, except author-date order) and is dropped for a shallow repository; the multi-pack-index (one file, or an incremental `multi-pack-index.d/` chain; every named pack present) answers `Odb::contains`. A file that fails its structural checks, or whose trailer is not the hash of its body, is ignored as a whole; `core.commitGraph=false` and `core.multiPackIndex=false` switch the use (and the `fsck` check) off. `is_ancestor` (merge-base --is-ancestor, branch --merged, fast-forward tests) skips commits whose generation number is below the candidate's. `fsck` verifies both files and exits 16 (graph) / 1 (index) like git for what it can: the load failures git words (too small, signature, version, hash version, chunk table, missing or mis-sized required chunks, fanout order), the checksum, and a graph that disagrees with the commits on tree, date or parents (`s_commit_graph_damage`). Not done: the generation findings of `commit-graph verify` (`commit-graph generation for commit ... < ...`, so a damaged parent or date field prints fewer lines than git), `GDA2`/Bloom data, a split chain in `fsck`, the content checks of `multi-pack-index verify` (`failed to load pack ...`, object offsets), the `die` cases of an index with a bad pack count, and generation pruning of the exclusion walk behind `rev-list A..B`, `--max-count` and `--since` (their answers do not depend on it).

## 3. Deliberate divergences

Differences that are decisions, not bugs. Each is documented where it happens in the source.

* `archive`: `export-subst` and `--worktree-attributes` are refused rather than approximated.
* Where git's own output is nondeterministic (the order of `remote:`/`fatal:` lines when a
  remote dies, the colour of an octopus `-.` line, which depends on uninitialised memory in
  git), the differential scenarios compare what is stable — stdout, exit status, or the
  decompressed content — and the source says so.

## Known defects (2026-10-27 review)

Broken in repositories real git wrote (`docs/reviews/2026-10-27-code-review.md`). Each line is
deleted in the commit that fixes it.

* **A2 (remainder)** — the recursive merge base is built for criss-cross histories, including when merging the bases conflicts in file content (the virtual base keeps the markers: nine characters wide at the first level, `Temporary merge branch 1/2`, git's `diff3` base names; `s_merge_criss_cross_markers`) and when the conflict is modify/delete or binary (the virtual base keeps the base's version, or the empty file for an add/add of binaries; `s_merge_criss_cross_nontext`). A base merge whose conflict is of another kind (distinct types / mode, submodule, directory/file, rename) is still refused by name (`fatal: merge with more than one merge base (...) is not implemented`, exit 128; `merge_criss_cross_refusal.rs`). `merge.conflictStyle` and git's marker labels are honoured by `merge`, `pull`, `cherry-pick`, `revert`, `rebase`, `stash apply/pop`, `checkout -m`/`switch -m` and `rebase --autostash` (`s_conflict_style_ops`: `HEAD`/`parent of <id> (<subject>)`, `Updated upstream`/`Stash base`/`Stashed changes`, the branch left as the `checkout -m` base); not covered: `merge-file`. The plain `merge` style now refines a conflict to the lines the sides really disagree on and joins conflicts at most three common lines apart, as git's zealous level does (`s_merge_conflict_gap`); `diff3`/`zdiff3` stay unjoined. A conflicting binary merge prints git's `warning: Cannot merge binary files: <path> (HEAD vs. <label>)`. Markers nested more than one virtual level deep (width `7 + 2 * depth`) are written from git's rule for the first level and not verified deeper.
* **A3 (remainder)** — `text`/`eol`/`crlf`/`binary`, `core.autocrlf`, `core.eol`, `core.safecrlf` and the attribute stack (`convert.rs`) apply on `add`/`status`/`checkout`, on the marked-up files a conflicted merge writes, on `stash` (tracked changes, untracked files, and their restore), on `commit -a`, and on the work tree side of `diff` and `blame` (`s_attributes_eol`, `s_attributes_eol_paths`); `ident`, `working-tree-encoding` and a configured `filter=` driver are refused by name on all of those paths (`convert_refusals.rs`). A `.gitattributes` that is not in the work tree (a sparse checkout) is read from the index (`s_attributes_sparse`). Not done: git reads attributes index-first when it writes files (`checkout` direction) and work-tree-first when it hashes them, while rivet always prefers the work tree copy; a bare repository has no attribute lookup at all (`check-attr --cached`); `grep` reads raw files like git, but `diff --no-index`, `apply` and `am` on the work tree do not convert; `commit -a` with unmerged paths refuses where git stages the conflicted files as they are.
* **A4** — short ids (`rev-parse`, `RevSpec::Abbrev`, the `index` lines of `apply --3way` / `am --3way`) are found through the loose fan-out directory and the packs' sorted id tables (`Odb::oids_with_hex_prefix`, `Odb::resolve_hex_prefix`, new and additive), digit by digit: a trailing `00` and an odd count of digits count (`s_abbrev_zero`, Rust test `hex_prefix_keeps_a_trailing_00_and_an_odd_digit`). `Odb::resolve_prefix` keeps its zero-filled `ObjectId` argument, so a prefix whose own last byte is `00` still reads as shorter there; nothing in the tree passes one any more. The fan-out also does not use a multi-pack-index or commit-graph.

## Flaky scenarios (fixed)

* `s_blame_flags` — git stamps a not-yet-committed line with the wall clock, so `blame -n g` and `blame --since=1800000000 g` printed a time that differed when the git and rivet runs straddled a second. Both now use `-s` (no author, no time); no other command in the scenario prints the clock. `s_attributes_eol_paths` had the same two `blame` calls on a dirty work tree and got the same `-s`; every blame/attributes/sparse/stash/conflict/rebase/merge/pull/worktree/status/diff scenario was scanned for a wall-clock stamp and none remains.

## Reporting a gap

If a command refuses with `not implemented`, that is this list, not a crash. Anything else —
a panic, a wrong result, a hang — is a bug worth an issue with the command line and the
repository state that produced it.
* **`name-rev`** — `--all` lists the commits in git's in-memory object-table order, reproduced by simulating the table (slot order of a hash table filled as git parses the refs and walks history from the tips: tags first, oldest first, first parent first; `s_name_rev_all`, Rust tests `table_tests`). That order is an accident of git's internals: a repository with a commit-graph file, grafts or `refs/replace` objects may order differently, and nothing but the differential runs vouches for the model. When several refs point at the same non-commit object (a tag object), git's choice among them depends on a binary search in its tip table; that case is refused by name instead of guessed. The commit-date cutoff optimisation is not reproduced (it does not change a name unless dates are badly skewed).
* **`checkout` / `switch` with several unmerged paths** — every unmerged path is named (`<path>: needs merge`, once each, index order) before `error: you need to resolve your current index first`, for `checkout`, `-B`, `--detach`, `-m` and `switch` (`s_checkout_needs_merge`). `switch` refuses while a merge, cherry-pick, revert, rebase, `am` or bisect is under way (`fatal: cannot switch branch while merging` + the `Consider "git merge --quit" or "git worktree add".` line; only merge and cherry-pick are compared), and a switch that succeeds drops a cherry-pick/revert with `warning: cancelling a cherry picking in progress`. A checkout that moves nothing (`HEAD`, the branch already checked out, `--detach`, `-m`) and `-B` stop at an unresolved index as in git; `checkout -b <new>` stands on it, prints no local changes, and every successful branch switch (`-b`, `-f`, `-m`, `--detach`) drops `MERGE_HEAD`/`MERGE_MODE`/`MERGE_MSG`/`AUTO_MERGE` (`s_checkout_unmerged_noop`).
