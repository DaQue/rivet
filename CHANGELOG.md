# Changelog

* 2026-11-03 — `clone -b <tag>` detaches `HEAD` at the tag's commit with git's detached-HEAD note, no branch or tracking config, bare or not (`s_clone_branch_tag`).
* 2026-11-03 — `clone` of an empty repository writes `branch.<default>.remote`/`merge` (not for `--bare`), as git does (`s_clone_empty_tracking`).
* 2026-11-01 — `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` in ISO form with a spaced offset are honoured and a bad value is git's `invalid date format` fatal, so commit ids match git's (G2 workflow parity green).
* 2026-11-01 — rebase interop proved against git: a plain (non-`-i`) rebase stopped on a conflict is continued, skipped or aborted by the other implementation in both directions with identical ids, reflogs and files (`s_rebase_interop_plain`); the GAPS bullet is closed.
