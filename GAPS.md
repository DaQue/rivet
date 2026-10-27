# Known gaps

rivet is a Git-compatible implementation, not a wrapper: every command below is compared
against the real `git` binary by the differential suite in `tools/difftest/scenarios.sh`
(currently 317 scenarios). This file lists what does **not** work yet, so that nobody has to
discover it by running a command that fails.

Last checked 2026-10-27, gate: 271 tests / 0 failed, 317 scenarios / 0 failures, `fmt`,
`clippy -D warnings` and `doc` clean.

Three different things get called "not working". They are separated here on purpose, because
only the first two are defects.

## 1. Not implemented at all

The command or subsystem is absent; it either fails at the argument parser or refuses by name.

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
  `--rules-file`, C-quoted input and output; `clean` with `-n`/`-f`/`-v`, `s_sparse_clean`); the sparse index does not. `--sparse-index` is refused by name before anything is
  touched, and an index that real git wrote with the `sdir` extension is refused on read
  (`not implemented yet`), because treating its directory entries as files reports every path under
  them as deleted (`crates/rivet-cli/tests/sparse_index.rs`). `--no-sparse-index` is accepted and
  does nothing.
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
  `--reference` writes the module's `objects/info/alternates` (rivet still copies every object, and does
  not read alternates); `--dissociate` leaves none. `-j`/`--jobs` is validated and accepted but runs sequentially.
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
* **`blame`** — `-M`/`-C` refuse while the file has uncommitted changes; no `--since`, `-S`, `-L :<funcname>` (refused by name). `-L` with `/regex/`, `^/regex/` and `<start>,/regex/` works (`s_blame_regex_range`); the regex is a basic one from rivet's own matcher, so the text of a *compile* error can differ from glibc's for a few malformed patterns (git says `Invalid regular expression` for `[`).
* **`grep`** — `-o`, `--heading`, `-z`/`--null`, `--break` and `--untracked` work (`s_grep_output_modes`, `s_grep_null_break`, `s_grep_untracked`; `--untracked` searches the non-ignored untracked files merged in path order, refuses `--cached`, and reads a revision as a path);
  `--break` over a binary file that matches is refused by name (git drops or keeps the notice depending on
  its worker threads; Rust test `grep_break_refuses_binary`). `--threads=<n>` / `--no-threads` are validated like git's and
  otherwise ignored (`s_grep_threads`). No `-P`, `--and`/`--or`/`--not`, `--no-index`.
* **`apply`** — `GIT binary patch` (from `diff --binary`) is not decoded; no fuzz, symlink
  patches, whitespace options or `--reject` together with `--3way`.
* **`format-patch` / `am`** — no cover letters, threading, RFC 2047 headers (so `-k` with a
  multi-line subject or a run of blanks is refused), scissors, `am --3way`.
* **`stash`** — `create`, `store` and `save` exist (`s_stash_stack`), as does `branch`
  (`s_stash_branch`), and `save`/`store` error text matches (`s_stash_errors`). Not matched: `-p`/`-S`/`--staged`
  are not implemented for any stash form (`save` refuses them by name).
* **`gc` / `repack`** — no `--cruft`, `--geometric`, `--window`/`--depth`,
  `--max-pack-size`, `--max-cruft-size`, `--expire-to`, `--keep-largest-pack`,
  `gc.bigPackThreshold`; `repack -k` over several packs lists unreachable objects in a
  different order from git.
* **`bisect`** — `start --first-parent`, `--reset-when-found` and path arguments are refused.
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
  `store`/`erase` (`s_credential_state`); the multi-round retry itself is not implemented (git re-runs the
  helper chain with the state after a rejected `continue=1` credential, rivet erases and fails), nor is
  `state[]` shown to a later helper's `get`. Not implemented: `credential.<url>.*` keys other than `helper`,
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

## 3. Deliberate divergences

Differences that are decisions, not bugs. Each is documented where it happens in the source.

* `archive`: `export-subst` and `--worktree-attributes` are refused rather than approximated.
* Where git's own output is nondeterministic (the order of `remote:`/`fatal:` lines when a
  remote dies, the colour of an octopus `-.` line, which depends on uninitialised memory in
  git), the differential scenarios compare what is stable — stdout, exit status, or the
  decompressed content — and the source says so.

## Known defects (2026-10-27 review)

Broken in repositories real git wrote (`docs/reviews/2026-10-27-code-review.md`). Each line is
deleted in the commit that fixes it. A15 is not in the queue yet.

* **A2** — criss-cross merge uses `bases.first()` and commits a tree git conflicts on (`branching.rs`).
* **A3** — `.gitattributes` / `core.autocrlf` give different blobs from git (`worktree/add.rs`, `checkout.rs`).
* **A5** — delta chains deeper than 50 cannot be read; errors carry an all-zero id (`pack/reader.rs`).
* **A6** — `.git/shallow` is never read; shallow clones fail `log`/`fsck`.
* **A7** — unknown `extensions.*` not refused; `preciousObjects` ignored by `gc`/`prune`/`repack -d`.
* **A8** — malformed-but-real identity lines abort `log` (`object/commit.rs`).
* **A9** — index v4 refused; split-index `link` extension misread.
* **A10** — `objects/info/alternates` is not read (`odb/mod.rs`).
* **A11** — no post-fetch connectivity check.
* **A12** — `gc` deletes old packs without fsyncing the new pack and idx first.
* **A13** — `inflate_at` / `apply_delta` size allocations from untrusted declared sizes; temp pack names are not unique.
* **A14** — `rev-parse --short HEAD` parses `HEAD` as the length; `history::abbrev` always prints 7 chars and scans the odb.
* **A1 (remainder)** — in-memory `RefStore` packed snapshot is not refreshed after `delete`/`rename` (the on-disk file is now correct).
* **A4 (remainder)** — short ids are matched by scanning `Odb::oids()` (no fanout use); `Odb::resolve_prefix` still reads a trailing `00` byte as fill (used by `revwalk.rs` `RevSpec::Abbrev`); hint abbreviations are a fixed 7 digits (A14).

## Reporting a gap

If a command refuses with `not implemented`, that is this list, not a crash. Anything else —
a panic, a wrong result, a hang — is a bug worth an issue with the command line and the
repository state that produced it.
