# PROGRESS

## Done
- `rivet-core::ops` (shared porcelain mechanics: identity, HEAD move + reflogs, checkout, reset, restore, merge).
- CLI handlers: `branch`, `switch`, `checkout`, `merge`.
- Differential harness: `tools/difftest/scenarios.sh [scenario...]` (real git vs rivet: transcripts + repo state).
- Core fixes found by the harness: `init` layout/config, config serialisation and section editing
  (`add/remove/remove_section/sections/keys` never matched), `merge_bases` (was a placeholder),
  signature tz sign, diff of root commits / file paths with trailing `/`, `--stat` layout, commit output + reflogs.

## Verified (differential, PASS)
s_basic_commit, s_branch_ops, s_switch_checkout, s_merge_conflict

## Remaining
- CLI stubs: log, show, diff, tag, reset, restore, check-ignore, config, clone, remote/fetch/pull/push, verify-pack, fsck.
- Scenarios not yet passing: s_merge_ff_and_true (needs log), and all others in tools/difftest/scenarios.sh.
