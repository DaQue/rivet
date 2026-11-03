# Changelog

* 2026-11-03 — `worktree add --orphan` makes an unborn branch work tree with git's option clashes and fatals; `--reason` without `--lock` is git's wording (`s_worktree_add_orphan`).
* 2026-11-03 — `rev-list --cherry` is `--right-only --cherry-mark --no-merges` (`s_rev_list_cherry_synonym`).
* 2026-11-03 — `diff-tree --stdin` reads its lines with no path limit once a tree-ish was given on the command line, as git does; a limit with no tree-ish still applies (`s_diff_tree_stdin_limit`).
* 2026-11-03 — `config --get-regexp` takes its pattern after `-t <type>`, `-t<type>`, `-f <file>`, `--file <file>` and `--file=<file>` (`s_config_get_regexp_after_opts`).
* 2026-11-03 — `blame --contents <file>` (`-` is standard input) blames that file as the final version of the path, its changed lines credited to `External file (--contents)` with git's `Version of <path> from <file>` summary (`s_blame_contents`).
* 2026-11-03 — `config --type=color` reads `no<attr>`/`no-<attr>` words (`nobold`, `no-ul`, ...): the positive attributes come first, then the sorted 22/23/24/25/27/29 codes (`s_config_color_no`).
* 2026-11-03 — `show --oneline`: one line with the abbreviated id, the last of `--oneline`/`--pretty`/`--format` wins and a later block layout abbreviates its `commit` line (`s_show_oneline`).
* 2026-11-03 — `clone -b <tag>` detaches `HEAD` at the tag's commit with git's detached-HEAD note, no branch or tracking config, bare or not (`s_clone_branch_tag`).
* 2026-11-03 — `clone` of an empty repository writes `branch.<default>.remote`/`merge` (not for `--bare`), as git does (`s_clone_empty_tracking`).
* 2026-11-01 — `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` in ISO form with a spaced offset are honoured and a bad value is git's `invalid date format` fatal, so commit ids match git's (G2 workflow parity green).
* 2026-11-01 — rebase interop proved against git: a plain (non-`-i`) rebase stopped on a conflict is continued, skipped or aborted by the other implementation in both directions with identical ids, reflogs and files (`s_rebase_interop_plain`); the GAPS bullet is closed.
* 2026-11-03 — `config -f`/`--file <path>` reads and writes one named file (a missing file is silent for `--get`, a fatal for `--list`), and two scopes together are git's `error: only one config file at a time`, exit 129 (`s_config_file`).
* 2026-11-03 — `config --get-color <slot> [<default>]` prints the escape sequence of the slot's colour or of the default word, nothing without either, git's errors for a bad value, a bad default and `--type` (`s_config_get_color`; a bare `key` is not compared, git crashes on it).
* 2026-11-03 — `config --type=int` on a write stores the number (`3k` becomes `3072`) and refuses text with git's `bad numeric config value '<v>' for '<key>': invalid unit` (`s_config_int_write`).
