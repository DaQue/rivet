# Known gaps

rivet is a Git-compatible implementation, not a wrapper: every command below is compared
against the real `git` binary by the differential suite in `tools/difftest/scenarios.sh`
(currently 216 scenarios). This file lists what does **not** work yet, so that nobody has to
discover it by running a command that fails.

Last checked 2026-10-24, gate: 229 tests / 0 failed, 212 scenarios / 0 failures, `fmt`,
`clippy -D warnings` and `doc` clean.

Three different things get called "not working". They are separated here on purpose, because
only the first two are defects.

## 1. Not implemented at all

The command or subsystem is absent; it either fails at the argument parser or refuses by name.

* **Credential helpers** — `credential.helper` is not consulted and there is no prompt flow.
  HTTPS credentials come from the URL/userinfo or the transport's own environment; an
  authenticating remote that needs a helper will fail.
* **Sparse checkout and partial clone** — `sparse-checkout`, `--filter`, promisor/partial
  clones and `checkout` honouring `skip_worktree` entries are not implemented. The index
  records the `skip-worktree` bit (see `IndexEntry::skip_worktree`), but no command sets it.
* **`rebase --interactive`** — also `--root`, `--rebase-merges`, `--exec`, `--keep-base` and
  signing. The grammar rejects them.
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

* **`log`** — one revision only (`log a b` is an argument error); `--color` is not implemented
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
* **`stash`** — no `-u`/`-a`/`-k`, pathspecs, `apply --index`, `branch`/`create`/`store`/`save`.
* **`clean`** — no `-i`/`-e`.
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
  `post-commit` and `post-rewrite amend` for `commit` (`--no-verify`/`-n`); `pre-merge-commit`,
  `prepare-commit-msg`, `commit-msg` and `post-merge` for `merge`; `post-checkout` for `switch`,
  `checkout` (branches and paths). Not run yet: anything on the `merge --continue`, `cherry-pick`,
  `revert`, `am`, `stash`, `pull`-induced merge paths beyond what `merge` shares, `rebase`
  (`pre-rebase`, `post-rewrite rebase`), `push` (`pre-push`), `clone`/`worktree add`
  (`post-checkout`), `reference-transaction`, `applypatch-msg`/`pre-applypatch`/`post-applypatch`,
  `pre-auto-gc`, `post-index-change`, and every server-side hook. Only `commit -m/-F` exists, so
  `prepare-commit-msg` only ever sees the `message` source. `GIT_DIR`/`GIT_WORK_TREE` are not
  exported to hooks (git does not either for the common layout; linked work trees are untested).
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
