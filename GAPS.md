# Known gaps

rivet is a Git-compatible implementation, not a wrapper: every command below is compared
against the real `git` binary by the differential suite in `tools/difftest/scenarios.sh`
(currently 364 scenarios). This file lists what does **not** work yet, so that nobody has to
discover it by running a command that fails.

Last checked 2026-10-31, gate: 339 tests / 0 failed, 364 scenarios / 0 failures, `fmt`,
`clippy -D warnings` and `doc` clean.

Three different things get called "not working". They are separated here on purpose, because
only the first two are defects.

## 1. Not implemented at all

The command or subsystem is absent; it either fails at the argument parser or refuses by name.

* **Split index** — read (`link` extension merged with `sharedindex.<hash>`, deletions, replacements, additions) and written (`core.splitIndex`, `splitIndex.maxPercentChange`, `update-index --[no-]split-index`; `s_split_index`, which has git read what rivet wrote and the reverse). Not done: `splitIndex.sharedIndexExpire` and cleaning up old `sharedindex.*` files, and rivet's choice of when to write a new shared index is a simplification of git's (additions only, against `maxPercentChange`). Index v4 is read and written, and `index.version` (2 to 4) / `feature.manyFiles` pick the version of a new index (`s_index_version`); git's `index.version` warning for an out-of-range value and its fatal for a non-numeric one are not reproduced.
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
  entry stays expanded in the file; `rivet config --get` does not read `config.worktree` (where `index.sparse` and
  `core.sparseCheckout*` live), so it reports them unset.
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
* **Rebase interop** — a rebase that rivet starts (plain or `-i`) can be resumed, skipped or aborted by real
  `git rebase --continue`/`--skip`/`--abort`, and the reverse, with identical ids, reflogs and files
  (`s_rebase_interop`: edit stop, squash, conflict, reword). rivet's `rebase-merge/` carries one extra file,
  `rivet-interactive`, which git ignores; a git-written state lacks it and rivet does not need it. Plain
  (non-`-i`) rebase states written by git are not compared.
* **`bundle-uri`** (beyond `clone --bundle-uri=<file>`) and SHA-256 bundles. Creating, listing, verifying and unbundling bundles works;
  fetching, cloning and `ls-remote` *from* a bundle file work (`s_bundle_clone`, `s_ls_remote`). `clone --bundle-uri` takes a file path
  or `file://` URL only (refs land in `refs/bundles/*`, `s_clone_bundle_uri`); other schemes warn and fall back to a plain clone, as an
  unreachable URI does in git. Bundle lists (`bundle.*` config), `fetch.bundleURI` and the `bundle-uri` protocol command are not
  implemented; a file that is not a bundle gets git's two warnings but not the `bad config line` error naming a random temp file.
  `ls-remote --sort=<key>` works for `refname` (also `lstrip`/`rstrip`/`strip`, a leading `-`, several `--sort`, `s_ls_remote_sort`);
  `version:`/`v:` over `refname` (`s_ls_remote_version_sort`) works too; `*` (ignore case), `refname:short`/`:dir`/`:base` and the object-data keys are refused by name, and git's own
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
  "commit or discard" hint; `s_status_moved_submodule`, `s_status_dirty_submodule`). A dirty nested submodule inside a
  submodule, and `status.submoduleSummary` / `diff.ignoreSubmodules` / `--ignore-submodules`, are not compared. `diff`/`commit -a`/`add -u`
  still skip gitlinks. `status` takes several literal pathspecs and reports their union (`s_status_multi_pathspec`); pathspec magic works there too (`:(exclude)`/`:!`/`:^`, `:(top)`/`:/`, `:(glob)`, `:(literal)`, `:(icase)`, bare wildcards, `..` in patterns, git's errors; `s_status_pathspec_magic`); `:(attr:...)` is refused by name. A wholly untracked directory named by a literal pathspec is reported as `dir/` (`s_status_untracked_dir_pathspec`). A pathspec outside the tree gets git's `outside repository at` error, plain or magic (`s_status_outside_pathspec`).

## 2. Implemented, but partial

The command works for the common case; the listed options or edge cases are missing or refused.

* **`log`** — takes any number of revisions and ranges (`s_log_multi`); `--color` paints `-p` and
  `diff --color` the patch with git's default palette (`s_diff_color`), `--name-*` are plain as in git.
  `--stat` is painted too (`s_log_stat_color`; `log`, `show`, `diff`). Refused by name (Rust tests in
  `tests/diff_color.rs`): any `color.diff` / `color.diff.<slot>` setting, a space before a tab in an added line (git highlights it), colour
  for the conflicted-path output of `diff`; `log --first-parent` diffs a merge against its first parent and an
  empty `--format=` works (`s_log_first_parent_merge_diff`); the last of `--oneline` / `--pretty` / `--format`
  wins and `--oneline` abbreviates the `commit` line of a later layout (`s_log_layout_order`; `log` only, read
  from the raw command line); `show` has no `--oneline` (`--pretty=oneline` works);
  `--graph` over several root commits exits 128 by design; `--graph` with `--date-order` or
  `--first-parent` orders differently from git, and `--graph --format=format:%s` draws an
  extra row.
* **`rev-list`** — `--cherry-pick` and `--cherry-mark` work (`s_rev_list_cherry`; `--cherry` and `log --cherry-mark`
  do not exist). `--children` works (`s_rev_list_children`). `--left-right`, `--left-only`
  and `--right-only` work (`s_rev_list_left_right`), and so does `--ancestry-path` without a
  commit argument (`s_rev_list_ancestry_path`); `--ancestry-path=<commit>` keeps the commit, its
  descendants and its ancestors (`s_rev_list_ancestry_path_commit`), flagging the ancestors in walk order
  as git does so skewed dates drop the same ones (`s_rev_list_ancestry_path_skew`).
* **`for-each-ref`** — `--shell`/`--perl`/`--python`/`--tcl` (and `-s`/`-p`, `--no-…`) work (`s_for_each_ref_quote`);
  `--stdin` and `--start-after` / `--no-start-after` work (`s_for_each_ref_stdin_start`);
  `--include-root-refs` / `--no-include-root-refs` work (`s_for_each_ref_root_refs`: `HEAD` and `[A-Z_]*_HEAD` files of the git
  directory, not `FETCH_HEAD`/`MERGE_HEAD`, git's "ignoring broken ref" warning, `%(symref)`, commit filters drop them); only the
  work tree's own git directory is scanned, so a linked work tree is not compared. `--sort=version:<key>`/`v:<key>` follows git's `versioncmp` (`s_for_each_ref_version_sort`); `versionsort.suffix` (and the deprecated `prereleasesuffix`) is honoured (`s_for_each_ref_version_suffix`, fuzzed against git); `ls-remote --sort=version:` honours it too (`s_ls_remote_version_suffix`; the repository's own config only, not global). Pattern and `--exclude` globs follow `wildmatch` with `WM_PATHNAME` (`s_for_each_ref_patterns`).
* **`blame`** — `-M`/`-C` refuse while the file has uncommitted changes. `-n`/`--show-number` (`s_blame_flags`), `--since`/`--after` (older commits become boundary commits; an unparsable date means now, as in git), `-S <revs-file>` (git's graft file: `<commit> <parent>...` lines, `bad graft data` errors) and `-L :<funcname>` / `-L ^:<funcname>` (`s_blame_funcname`) work. `-L :<funcname>` uses git's default function-line rule or a `diff=<driver>` with `diff.<driver>.xfuncname` / `funcname` (negated patterns included); a driver that names one of git's built-in userdiff drivers (`diff=cpp`, `java`, ...) is refused by name (`blame_funcname_refusal.rs`). `-L` with `/regex/`, `^/regex/` and `<start>,/regex/` works (`s_blame_regex_range`); the regex is a basic one from rivet's own matcher, so the text of a *compile* error can differ from glibc's for a few malformed patterns (git says `Invalid regular expression` for `[`). Not done: `--contents`, `--reverse`, the other rev-list limits (`--until`, `--max-count`, ...).
* **`grep`** — `-o`, `--heading`, `-z`/`--null`, `--break` and `--untracked` work (`s_grep_output_modes`, `s_grep_null_break`, `s_grep_untracked`; `--untracked` searches the non-ignored untracked files merged in path order, refuses `--cached`, and reads a revision as a path);
  `--break` over a binary file that matches is refused by name (git drops or keeps the notice depending on
  its worker threads; Rust test `grep_break_refuses_binary`). `--threads=<n>` / `--no-threads` are validated like git's and
  otherwise ignored (`s_grep_threads`). No `-P`, `--and`/`--or`/`--not`, `--no-index`.
* **`apply`** — `GIT binary patch` (`diff --binary`, literal and delta blocks) is decoded, with `-R`
  taking the reverse block; a *damaged* data block is refused with a plain message where git prints
  `corrupt binary patch at <file>:<line>` and carries on. `am --3way` still refuses binary patches. `-C<n>` reduces
  context like git (with its `Context reduced to (l/t) to apply fragment at N` note), symlinks can be created,
  deleted and retargeted (`wrong type` as git). No whitespace options or `--reject` together with `--3way`.
* **`format-patch` / `am`** — `--cover-letter` (`0000-cover-letter.patch`: shortlog wrapped at 72 columns,
  the range's diffstat and summary, numbering `0/N`), `--thread[=shallow|deep]`, `--no-thread` and `--in-reply-to`
  (`Message-ID`, `In-Reply-To`, `References`; a deep thread lists every earlier message), and RFC 2047 encoding of
  non-ASCII names and subjects (folded at 76 columns, the address moved to its own line past 78, the MIME headers
  added for a non-ASCII subject or message, ASCII names with a special quoted) are compared with git
  (`s_format_patch_cover`; the message ids carry the clock, so the scenario normalises them). `-k` with a
  multi-line or blank-run ASCII subject is still refused, as are attachments, path limiting, `format.coverLetter`,
  `format.thread`, `--description-file`, `-v<n>`, `--range-diff`, scissors. Found on the way and not fixed: `commit`
  keeps the trailing `"` of an author name where git's ident cleanup drops it, and the reflog message of a commit
  whose subject has a tab stops at the tab. `am --3way`/`-3`, `--no-3way`, `am.threeWay` and `--[no-]rerere-autoupdate` work (`s_am_3way`, `s_am_3way_rerere`); `am --show-current-patch` is still absent.
* **`stash`** — `create`, `store` and `save` exist (`s_stash_stack`), as does `branch`
  (`s_stash_branch`), and `save`/`store` error text matches (`s_stash_errors`). `push`/`save` `-S`/`--staged` (the stash is the index; the staged changes leave the work tree through a
  reverse `apply` with one line of context and are reset in the index, `-k` keeps the index, a pathspec
  only selects which paths are reset in the index, `Can't use --staged and --include-untracked or --all`,
  `No staged changes`; `s_stash_staged`, including `apply`, `apply --index` and `pop`). A staged binary file or
  submodule is refused by name. Not matched: `-p`/`--patch` (interactive hunk selection) is refused by name
  for `push` and `save` (`stash_patch_refusal.rs`); there is no `--pathspec-from-file` and no `-U`.
* **`gc` / `repack`** — no `--cruft`, `--geometric`, `--window`/`--depth`,
  `--max-pack-size`, `--max-cruft-size`, `--expire-to`, `--keep-largest-pack`,
  `gc.bigPackThreshold`; `repack -k` over several packs lists unreachable objects in a
  different order from git.
* **`bisect`** — `start --first-parent`, path arguments to `start` (commits that leave the paths alone are never tested; a merge identical to one parent follows only that parent) and `--reset-when-found[=original|found]` on `start` and `run` are compared with git, by hand and with `run` (`s_bisect_flags`: `BISECT_FIRST_PARENT`, `BISECT_NAMES`, `BISECT_RESET_WHEN_FOUND`, `Bisecting: -1 revisions left`, `No testable commit found` ending the bisection, a silent reset to the start or to the commit found, and the `BISECT_TERMS` file git leaves after a hand-driven finish). Not matched: a path that selects only part of a merge's history is simplified like git's default only for the cases in the scenario (no `--full-history`, no parent rewriting across non-merge commits), and `bisect run` leaves `BISECT_RESET_WHEN_FOUND` behind when the run stops before finding.
  `visualize`/`view` run `log` over the bad commit and `^` each good one, with git's quirk that
  `--bisect` is appended after the caller's words (`s_bisect_visualize`); the `gitk` form (a display
  and `gitk` on `PATH`) and a first argument that is not an option are refused by name
  (`bisect_visualize_refusals.rs`); an unknown option gets rivet `log`'s clap error, not git's
  `fatal: unrecognized argument`.
* **`archive`** — `export-subst` is refused (deliberate); gzip and deflated-zip bytes are not
  byte-identical to git's (decompressed content is); `--remote` works for local repositories
  and `file://`, not over a network URL.
* **`submodule summary`** — compares gitlinks only; a submodule whose `.gitmodules` entry is
  missing is reported as git does, but submodule `HEAD` discovery outside
  `Repository::open_or_discover` (`core.worktree` variants) and the
  `submodule.<name>.active`/`ignore` settings are not consulted.
* **`symbolic-ref`** — read, `--short`, `-q`, `--[no-]recurse`, write (with `-m`) and
  `--delete` work. A one-level lowercase *target* (`symbolic-ref FOO foo`) is refused by name
  (`not implemented: symbolic-ref to the one-level target …`, exit 128, nothing written; Rust test
  `a_one_level_target_is_refused_by_name_and_nothing_is_written`) because `RefName` cannot hold it,
  and git's quirk of writing a ref with a malformed *name*
  (`refs/heads/a..b`) is not reproduced (rivet reports `error: refusing to update ref`).
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
  system config. Not run yet: `pre-auto-gc`, `post-index-change`, and every server-side hook. `hook run` runs one hook (`--ignore-missing`, `--allow-unknown-hook-name`, `--to-stdin`; `-j` is accepted and ignored); `hook list` lists the hooks directory only (`-z`, `--show-scope`); a repository with `hook.<name>.command`/`.event` in its config is refused by name (`fatal: hooks defined in the configuration ... not supported`, exit 128), and `hook run` still ignores such hooks. Only `commit -m/-F` exists, so `prepare-commit-msg` only ever sees the `message`
  source (and `merge`). `GIT_DIR`/`GIT_WORK_TREE` are not exported to hooks (git does not either
  for the common layout; linked work trees are untested).
* **`reference-transaction`** — deliberately not implemented: git 2.56 runs it for every ref write with
  four states (`preparing`, `prepared`, `committed`, `aborted`), `ref:<target>` in the stdin for
  symbolic updates (`switch` writes `<null> ref:refs/heads/x HEAD`), a `HEAD` entry next to the branch
  entry in `preparing` for a commit, and internal transactions on pseudo-refs (`AUTO_MERGE` is
  prepared, aborted and committed again around every commit and switch; `branch -m` aborts one
  transaction and redoes it). A hook that saw only some of those events would mislead whoever relies
  on it (replication, auditing), so rivet runs none until the whole set can be matched.
* **Found by `s_hooks_clone`, not hooks** — `worktree add <path> <name>` does not create a local branch
  from a unique remote-tracking branch (`fatal: invalid reference`), and `worktree add -b <new>
  origin/<x>` does not set up `branch.<new>.remote`/`merge`; `clone` of an empty repository does not
  write `branch.<default>.remote`/`merge`; `clone -b <tag>` is refused.
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
* **`pull --rebase <remote> <branch>`** — with an explicit refspec, rebases onto the fetched
  tip rather than the fork point.
* **Network protocol** — v2 capabilities other than `object-format=sha1` are not implemented;
  git follows tags after a fetch with an explicit refspec where rivet does not.
* **`update-ref --stdin`** — `symref-update`, `symref-create`, `symref-delete` and `symref-verify` (line and `-z` framing, git's validation text, the `ref`/`oid` old values, `symref-delete`/`-verify` refused in deref mode, a symref update through a symbolic ref pointing both the named ref and the end of the chain at the target), `option no-deref` for the next command only, and `-0`/`--batch-updates` (what holds is applied, the rest comes back as `rejected <ref> <new> <old> <reason>` with `incorrect old value provided`, `reference does not exist`, `reference already exists` and `expected symref but found regular ref`, checked when the batch commits) are compared with git (`s_update_ref_symref`); `delete` removes the ref while holding its lock (`RefStore::delete_holding`). Not done: a batch rejects a name conflict (`refname conflict`) where rivet stops with a fatal error; C-style quoted fields in line mode; the duplicate and old-value checks of a symref update through a chain are only verified for a plain symbolic ref; git refuses commands after `prepare` (`prepared transactions can only be closed`). Found on the way: `for-each-ref %(symref)` prints the direct target of a chain of symbolic refs, git the final one.
* **`commit-tree`** — `-S`/`--gpg-sign` and a non-UTF-8 `i18n.commitEncoding` (git adds an `encoding` header) are refused by name (`commit-tree: signing (-S) is not supported`, exit 128; `commit_tree_refusals.rs`). Relative date words in `GIT_AUTHOR_DATE` (`2.days.ago`) are rejected as `invalid date format`, as `parse_git_date` does not read them.
* **`diff-tree`** — `--stdin` (one commit, `<commit> <parent>...` or `<tree> <tree>` per line; a line that names nothing is echoed; `error: Need exactly two trees, separated by a space`, `Object <id> not a tree`), `--numstat` (also `-z`), `--patch-with-raw`, `--patch-with-stat` and `--full-index`, and the combinations of `--raw`, `--numstat`, `--stat` and `-p` (blank line, NUL under `-z`, before the patch) are compared with git (`s_diff_tree_opts`). `-M[<n>]`/`--find-renames[=<n>]`, `-C[<n>]`/`--find-copies[=<n>]`, `--find-copies-harder` and `--no-renames` are compared with git for the raw, `--name-status`, `--name-only`, `-z`, `--stat`, `--numstat` and `-p` outputs (`R<nnn>`/`C<nnn>`, `copy from/to`, `similarity index`, a deleted file copied twice is renamed to the last destination; `s_diff_tree_renames`). `-c`/`--cc` is refused by name (`diff-tree: <option> is not implemented`, exit 128; `diff_tree_refusals.rs`). Not done for renames: `-B` break detection, `diff.renameLimit` beyond the fixed 1000 x 1000, a copy source filtered by a path limit under `--find-copies-harder` is only checked for the plain path-prefix case, and `diff`/`log`/`show` do not take `-C` (only `-M` by default). Not done: `--stdin` with a path limit given as a positional tree-ish (`--stdin <tree> -- <path>`: git then ignores the limit), a line with several parents without `-m`, `--stdin` lines naming a tag. `-p` of a symlink replaced by a regular file (or the reverse) prints git's deletion followed by an addition, in `diff`, `diff-tree`, `show` and `log -p` alike (`s_diff_mode_swap`; `FileDiff::split` is filled where the diff is built, so the raw, name-status and stat outputs keep their single `T` entry). `diff-index`, `diff-files` and `diff --raw`/`-p` do not exist as commands or options. `--first-parent` is accepted and changes nothing, as in git 2.x without `-m`.
* **`mktree`** — a mode with a sign or wider than 32 bits is refused by name (`mktree: a negative mode is not implemented`, `mktree: a mode wider than 32 bits is not implemented`, exit 128; `mktree_refusals.rs`), because git's `strtoul` reading of such a mode wraps. Leading whitespace and `+` before a mode are accepted, as in git. 
* **`rerere`** — `status`, `remaining`, `diff`, `gc`, `clear`, `forget` and the hook that `merge` (also `--squash`, `--continue`), `cherry-pick`, `revert`, `rebase`, `commit`, `stash apply/pop`, `am --3way`/`--continue` and the autostash behind `checkout -m`, `switch -m` and `rebase --autostash` run are byte-compatible with git's `rr-cache` and `MERGE_RR` (`s_rerere`, `s_rerere_stash`, `s_am_3way_rerere`). `forget` also sees paths already staged: the resolve-undo (`REUC`) extension is written whenever a stage-0 entry or a removal replaces conflict stages, read back by `ls-files --resolve-undo`, `update-index --unresolve`, `--clear-resolve-undo` and `rerere forget` (in memory, as git does), kept by `commit`, and dropped by branch switches and merges like git (`s_resolve_undo`, `s_rerere_forget_staged`). Not done: the `conflict-marker-size` attribute and per-path `merge=` drivers are not honoured but refused by name where git would act on them (`merge`, `pull`, `cherry-pick`, `revert`, `rebase`, `stash`, `checkout -m`, `am -3`: `<path>: the \`merge=union\` attribute is not supported by rivet; refusing to merge ...`, `merge_attr_refusals.rs`); a `merge=<name>` with no `merge.<name>.driver` merges as text like git; `rerere` itself still uses marker width 7; the replays of `checkout -m` and `rebase --autostash` are silent and leave `MERGE_RR` removed / empty as git does, but that was verified for one conflicting file only; `gc.rerereResolved`/`gc.rerereUnresolved` accept an integer number of days, `now` or `never` only, not git's date expressions.
* **`update-index`** — implemented: paths, `--add`/`--remove`/`--replace`/`--force-remove`/`--info-only`, `--refresh`/`--really-refresh` (`-q`, `--unmerged`, `--ignore-submodules`, `--ignore-missing`), `--cacheinfo` (both spellings), `--index-info`, `--stdin` with `-z`, `--chmod`, `--[no-]assume-unchanged`, `--[no-]skip-worktree`, `--clear-resolve-undo`, `--force-write-index`, `--verbose`, and the format options (`s_update_index`, `s_split_index`, `s_index_version`). `--unresolve` puts the recorded stages back (all following words are paths). Refused by name (`update-index: --<opt> is not implemented`, exit 128; `update_index_refusals.rs`): `--again`/`-g`, `--[no-]untracked-cache` and its test/force forms, `--fsmonitor*`, and a `--cacheinfo` of mode `040000` (git stores a sparse-directory entry). Not reproduced: git's skipping of the object write when the entry already matches the file's stat data, option-name abbreviations (`--cache` for `--cacheinfo`). `add -N` writes the intent-to-add bit (v3 index, empty blob stored); `status`, `diff`, `write-tree`, `commit`, `commit -a`, `reset` and `rm --cached` treat such an entry like git (`s_add_intent_to_add`), but `checkout`, `stash`, `restore` and `mv` on one are untested.
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
* **A4 (remainder)** — short ids are matched by scanning `Odb::oids()` (no fanout use); `Odb::resolve_prefix` still reads a trailing `00` byte as fill (used by `revwalk.rs` `RevSpec::Abbrev`).

## Flaky scenarios (fixed)

* `s_blame_flags` — git stamps a not-yet-committed line with the wall clock, so `blame -n g` and `blame --since=1800000000 g` printed a time that differed when the git and rivet runs straddled a second. Both now use `-s` (no author, no time); no other command in the scenario prints the clock. `s_attributes_eol_paths` had the same two `blame` calls on a dirty work tree and got the same `-s`; every blame/attributes/sparse/stash/conflict/rebase/merge/pull/worktree/status/diff scenario was scanned for a wall-clock stamp and none remains.

## Reporting a gap

If a command refuses with `not implemented`, that is this list, not a crash. Anything else —
a panic, a wrong result, a hang — is a bug worth an issue with the command line and the
repository state that produced it.
* **`name-rev`** — `--all` is refused by name (`name-rev: --all is not implemented`; git lists commits in its in-memory object-table order). When several refs point at the same non-commit object (a tag object), git's choice among them depends on a binary search in its tip table; that case is refused by name instead of guessed. The commit-date cutoff optimisation is not reproduced (it does not change a name unless dates are badly skewed).
* **`checkout` / `switch` with several unmerged paths** — git prints `<path>: needs merge` for every unmerged path before `error: you need to resolve your current index first`; rivet prints the first one only (found while writing `s_resolve_undo`, which resolves all conflicts before it switches).
