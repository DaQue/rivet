# Known gaps

rivet is a Git-compatible implementation, not a wrapper: every command below is compared
against the real `git` binary by the differential suite in `tools/difftest/scenarios.sh`
(currently 246 scenarios). This file lists what does **not** work yet, so that nobody has to
discover it by running a command that fails.

Last checked 2026-10-25, gate: 240 tests / 0 failed, 246 scenarios / 0 failures, `fmt`,
`clippy -D warnings` and `doc` clean.

Three different things get called "not working". They are separated here on purpose, because
only the first two are defects.

## 1. Not implemented at all

The command or subsystem is absent; it either fails at the argument parser or refuses by name.

* **`rebase -q`** prints `warning: skipped previously applied commit <id>`; git is silent under `-q`.
* **Partial clone** — `--filter` and promisor/partial clones are not implemented.
* **Sparse checkout** — `sparse-checkout init`/`set`/`add`/`list`/`reapply`/`disable`/`check-rules` exist
  (cone mode and `--no-cone`, `--stdin`, `--skip-checks`; `check-rules` with `-z`, `--cone`/`--no-cone`,
  `--rules-file`, C-quoted input and output); `clean` and the sparse index (`--sparse-index`) do not.
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
  ignore the definition: `clean`/`update-index` details, `am`, `bisect`, and `git sparse-checkout set`
  leaving a directory that holds untracked files (the warning is printed, git also tries to remove it).
  `check-rules` run with a pattern file but `core.sparseCheckout` off, and a path starting with `/` in
  non-cone mode, are not compared.
* **`rebase --interactive`** — `pick`/`reword`/`edit`/`squash`/`fixup`/`drop`/`noop`, the editable todo list,
  `--continue`/`--skip`/`--abort` work (`s_rebase_i`, `s_rebase_i_stops`, `s_rebase_i_conflict`).
  Refused by name, before anything is touched: `--root`, `--rebase-merges`, `--exec`/`-x`, `--keep-base`,
  signing, `--edit-todo` outside an interactive rebase (rivet's plain rebase has no todo to edit),
  `--autosquash` without `-i` (git runs it as an unedited interactive rebase),
  `--autosquash` over an `amend!` commit that has a target (needs `fixup -C`) or a `fixup!` that names
  another fixup commit by id (`s_rebase_autosquash` covers the rest, plus `--no-autosquash` and
  `rebase.autoSquash`), and the todo commands `exec`, `break`, `label`, `reset`, `merge`,
  `update-ref` and `fixup -C`/`-c`. An empty message from the editor aborts the whole rebase back to the
  original tip (git stops with the commit pending). The editor sees the message plus git's `#` footer, not
  the `git status` template. `post-rewrite` lines for a squash chain are not compared with git.
* **Rebase interop** — a rebase that rivet starts (plain or `-i`) can be resumed, skipped or aborted by real
  `git rebase --continue`/`--skip`/`--abort`, and the reverse, with identical ids, reflogs and files
  (`s_rebase_interop`: edit stop, squash, conflict, reword). rivet's `rebase-merge/` carries one extra file,
  `rivet-interactive`, which git ignores; a git-written state lacks it and rivet does not need it. Plain
  (non-`-i`) rebase states written by git are not compared.
* **`bundle-uri`**, SHA-256 bundles, and `clone`/`ls-remote` from a bundle file. Creating,
  listing, verifying and unbundling bundles works; fetching *from* a bundle works only as a
  remote URL.
* **`rev-parse --git-common-dir`** and `--git-dir` with no argument are not in the frozen
  grammar.
* **Submodule advance/update modes** — `update --remote`, `--merge`, `--rebase`, `--depth`,
  `--reference`, `--filter`, `--single-branch`, `-j` and the `merge`/`rebase` strategies are
  refused with an explicit "not implemented". `checkout` is the only strategy.

## 2. Implemented, but partial

The command works for the common case; the listed options or edge cases are missing or refused.

* **`log`** — takes any number of revisions and ranges (`s_log_multi`); `--color` is not implemented
  (`-p`, `--stat`, `--name-*` under `--color=always` refuse rather than degrade);
  `--graph` over several root commits exits 128 by design; `--graph` with `--date-order` or
  `--first-parent` orders differently from git, and `--graph --format=format:%s` draws an
  extra row.
* **`rev-list`** — no `--skip`, `--max-age`/`--min-age`, `--left-right`, `--cherry-pick`,
  `--ancestry-path`, `--children`.
* **`for-each-ref`** — no `--shell`/`--perl`/`--python`/`--tcl`, `--stdin`, `--start-after`,
  `--include-root-refs`, `version:`/`v:` sort.
* **`blame`** — `-M`/`-C` refuse while the file has uncommitted changes; no `--incremental`,
  `-w`, `--since`, `-S`, regex `-L`.
* **`grep`** — no `-P`, `-o`, `--and`/`--or`/`--not`, `--heading`, `-z`, `--threads`,
  `--untracked`, `--no-index`.
* **`apply`** — `GIT binary patch` (from `diff --binary`) is not decoded; no fuzz, symlink
  patches, whitespace options or `--reject` together with `--3way`.
* **`format-patch` / `am`** — no cover letters, threading, `-k`, RFC 2047 headers,
  `--directory`, scissors, `am --3way`.
* **`stash`** — `create`, `store` and `save` exist (`s_stash_stack`), as does `branch`
  (`s_stash_branch`). Not matched: `save` with an unknown option exits 2 with clap's text (git prints its
  usage and exits 129), `save -- <words>` drops the `--` (git keeps it in the message), `store` of a
  non-commit object prints only the `fatal:` line (git adds `error: object <id> is a tree, not a
  commit`), and `-p`/`-S`/`--staged` are not implemented for any stash form.
* **`gc` / `repack`** — no `--cruft`, `--geometric`, `--window`/`--depth`,
  `--max-pack-size`, `--max-cruft-size`, `--expire-to`, `--keep-largest-pack`,
  `gc.bigPackThreshold`; `repack -k` over several packs lists unreachable objects in a
  different order from git.
* **`bisect`** — `start --first-parent`, `--reset-when-found` and path arguments are refused;
  `visualize` is not covered by a scenario.
* **`archive`** — `export-subst` is refused (deliberate); gzip and deflated-zip bytes are not
  byte-identical to git's (decompressed content is); `--remote` works for local repositories
  and `file://`, not over a network URL.
* **`submodule summary`** — compares gitlinks only; a submodule whose `.gitmodules` entry is
  missing is reported as git does, but submodule `HEAD` discovery outside
  `Repository::open_or_discover` (`core.worktree` variants) and the
  `submodule.<name>.active`/`ignore` settings are not consulted.
* **`symbolic-ref`** — read, `--short`, `-q`, `--[no-]recurse`, write (with `-m`) and
  `--delete` work. A one-level lowercase *target* (`symbolic-ref FOO foo`) is refused because
  `RefName` cannot hold it, and git's quirk of writing a ref with a malformed *name*
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
  `store`/`erase`) work (`s_credential_keys`). Not implemented: helper output `authtype`/`credential`/
  `state` (multi-round auth), `url=` (URL rewriting by a helper), `credential.<url>.*` keys other than `helper`,
  `useHttpPath` and `username`, `http.proxy` credentials, `credential.interactive`, the
  `git credential` command, and credentials for `ssh://` (the ssh program asks). Covered by
  `s_credential` against a local Basic-auth server; a `401` that first appears on a POST (not the
  discovery request) is not retried.
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

## Reporting a gap

If a command refuses with `not implemented`, that is this list, not a crash. Anything else —
a panic, a wrong result, a hang — is a bug worth an issue with the command line and the
repository state that produced it.
