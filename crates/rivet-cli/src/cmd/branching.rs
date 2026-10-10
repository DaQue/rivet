//! Branching commands: `branch`, `switch`, `checkout` and `merge`.
//!
//! The group that moves `HEAD` and joins histories. `branch` creates and deletes refs, `switch`
//! and `checkout` move the work tree onto one, and `merge` combines two. Everything here is
//! destructive in at least one mode, and every entry point is required to do its safety check
//! *before* it writes.
//!
//! *Phase 6 for `branch`/`switch`/`checkout`, Phase 9 for `merge` (plan §4).*

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::{ConflictStyleArg, GlobalArgs};

/// Arguments to `rivet branch`.
///
/// # Caveats
///
/// * **The command has seven modes and only one of them is "list".** With no positional and no
///   flag it lists; with `-d`/`-D`/`-m`/`-M`/`-c`/`-C` it acts on existing branches; with one
///   positional it creates one. A handler must resolve the mode once, before touching the ref
///   store, or a mistyped flag turns a create into a delete.
/// * **`delete` and `force_delete` differ in one check and the difference is unrecoverable.**
///   `-d` refuses to delete a branch whose commits are not merged into the current branch (or
///   into its upstream); `-D` deletes it anyway and the commits become unreachable except
///   through the reflog. A `-D` implemented as a `-d` that skips the error is fine; a `-d`
///   implemented as a `-D` loses work silently.
/// * **`force` (`-f`) moves an existing branch, and it is not the same as `-D`.** `-f` resets
///   a branch to a new commit, discarding the commits it pointed at; `-D` deletes the ref
///   entirely. Accepting one for the other produces a repository that looks the same and a
///   reflog that says something completely different.
/// * **`remotes` lists remote-tracking refs, and `all` lists both.** They are different
///   prefixes — `refs/heads/` and `refs/remotes/` — and a listing that used
///   `RefStore::list_all` for the plain form would show the user branches they cannot check out
///   by short name.
/// * **`set_upstream` writes config, not a ref.** It sets `branch.<name>.remote` and
///   `branch.<name>.merge`, and it must refuse when the named upstream does not exist, or
///   `rivet status` later reports a branch as tracking something that is not there.
/// * **A branch name that collides with an existing tag is a warning, not an error.** Git allows
///   it and the ambiguity is resolved in favour of the branch by `refs/heads/` being searched
///   first; a handler that refused would reject a repository Git accepts.
/// * **The default branch name comes from `init.defaultBranch`**, not from the literal `main`,
///   and the lookup happens at creation time so that a user's setting is honoured.
#[derive(Debug, clap::Args)]
pub struct BranchArgs {
    /// The branch to create, or, with `-d`, `-m` or `-c`, the branch to act on.
    #[arg(value_name = "BRANCH")]
    pub name: Option<String>,

    /// The commit the new branch should point at. Defaults to `HEAD`.
    #[arg(value_name = "START-POINT")]
    pub start_point: Option<String>,

    /// Delete a branch that is fully merged.
    #[arg(short = 'd', long = "delete", requires = "name")]
    pub delete: bool,

    /// Delete a branch even when it is not fully merged.
    #[arg(short = 'D', requires = "name")]
    pub force_delete: bool,

    /// Rename a branch, or the current branch when no name is given.
    #[arg(short = 'm', long = "move")]
    pub rename: bool,

    /// Rename a branch, overwriting the destination if it exists.
    #[arg(short = 'M', requires = "name")]
    pub force_rename: bool,

    /// Copy a branch rather than renaming it.
    #[arg(short = 'c', long = "copy")]
    pub copy: bool,

    /// Copy a branch, overwriting the destination if it exists.
    #[arg(short = 'C', requires = "name")]
    pub force_copy: bool,

    /// Move an existing branch to a different commit.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// List branches whose name matches this shell pattern.
    #[arg(long = "list", value_name = "PATTERN")]
    pub list: Option<Option<String>>,

    /// Print the branch that `HEAD` points at.
    #[arg(long = "show-current")]
    pub show_current: bool,

    /// List remote-tracking branches as well as local ones.
    #[arg(short = 'a', long = "all")]
    pub all: bool,

    /// List remote-tracking branches only.
    #[arg(short = 'r', long = "remotes")]
    pub remotes: bool,

    /// Print the upstream branch of each listed branch, in `refs/remotes/…` form.
    #[arg(long = "format", value_name = "FORMAT")]
    pub format: Option<String>,

    /// Set the upstream of the new branch.
    #[arg(short = 'u', long = "set-upstream-to", value_name = "UPSTREAM")]
    pub set_upstream: Option<String>,

    /// Do not set the upstream, even when `branch.autoSetupMerge` says to.
    #[arg(long = "no-track")]
    pub no_track: bool,

    /// List the branches whose commits are already merged into this one.
    #[arg(long = "merged", value_name = "COMMIT")]
    pub merged: Option<Option<String>>,

    /// List the branches whose commits are not yet merged into this one.
    #[arg(long = "no-merged", value_name = "COMMIT")]
    pub no_merged: Option<Option<String>>,

    /// Print the commit id of each listed branch and nothing else.
    #[arg(long = "points-at", value_name = "OBJECT")]
    pub points_at: Option<String>,
}

/// Arguments to `rivet switch`.
///
/// # Caveats
///
/// * **`switch` only ever moves between branches**, and refuses a commit id or a tag. That is
///   the entire point of its existence next to `checkout`: `rivet switch main` and
///   `rivet checkout main` do the same thing, but `rivet switch HEAD~3` is an error where
///   `rivet checkout HEAD~3` detaches. A handler that accepted a raw revision here would erase
///   the distinction the command was added for.
/// * **`detach` is the opt-in that allows a commit id**, and it and `create` are mutually
///   exclusive — `-c` names a branch to create and `--detach` says there is no branch.
/// * **`force` is not a synonym for `--discard-changes`.** `-f` moves `HEAD` even with a dirty
///   work tree and *may* lose changes; `--discard-changes` is the same effect with the
///   intention made explicit. Both exist in Git and both are preserved here.
/// * **`merge` makes the switch a merge rather than a reset**, carrying local modifications
///   into the target. Implementing it as a plain switch loses uncommitted work, and implementing
///   a plain switch as a merge leaves conflict markers in a file the user did not touch.
/// * **`create` from a remote-tracking branch sets the upstream automatically** — that is what
///   `rivet switch -c local origin/remote` is for — and `--no-track` turns it off. The
///   automatic behaviour comes from `branch.autoSetupMerge`, which is read from config and must
///   not be hard-coded.
#[derive(Debug, clap::Args)]
pub struct SwitchArgs {
    /// The branch to switch to.
    #[arg(value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Create the branch, at `--start-point` or at `HEAD`, before switching to it.
    #[arg(short = 'c', long = "create", value_name = "BRANCH")]
    pub create: Option<String>,

    /// Force the creation of a branch that already exists.
    #[arg(short = 'C', value_name = "BRANCH")]
    pub force_create: Option<String>,

    /// The commit to create the new branch at.
    #[arg(long = "start-point", value_name = "COMMIT")]
    pub start_point: Option<String>,

    /// Detach `HEAD` at the same commit as the named branch.
    #[arg(short = 'd', long = "detach")]
    pub detach: bool,

    /// Detach `HEAD` at the same commit as the named branch, going back this many commits first.
    #[arg(long = "orphan", value_name = "BRANCH")]
    pub orphan: Option<String>,

    /// Set the upstream of the new branch, or of the switched-to branch with `--track`.
    #[arg(short = 't', long = "track")]
    pub track: bool,

    /// Create the branch without an upstream, even when `branch.autoSetupMerge` says to.
    #[arg(long = "no-track")]
    pub no_track: bool,

    /// Throw away local modifications rather than refusing to switch.
    #[arg(short = 'f', long = "force", alias = "discard-changes")]
    pub force: bool,

    /// Carry local modifications into the target branch with a three-way merge.
    #[arg(short = 'm', long = "merge")]
    pub merge: bool,

    /// Keep the merge state on a failed `--merge` so the user can resolve it.
    #[arg(long = "conflict", value_name = "STYLE", value_enum)]
    pub conflict: Option<ConflictStyleArg>,

    /// Show what would happen, and change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Print a summary of the switch.
    #[arg(long = "progress")]
    pub progress: bool,

    /// Print nothing but the branch name.
    #[arg(long = "guess")]
    pub guess: bool,
}

/// Arguments to `rivet checkout`.
///
/// # Caveats
///
/// * **The disambiguating rule between the branch form and the paths form is Git's and it is
///   subtle**: `rivet checkout <branch>` moves `HEAD`, `rivet checkout <path>` restores a file,
///   and `rivet checkout <tree-ish> <paths>` does the second. The presence of `--` settles it,
///   a leading `refs/` settles it, and otherwise a name that matches a branch is a branch.
///   Getting this wrong means `rivet checkout main` either detaches or deletes a file called
///   `main`, so the rule must be implemented once and not guessed at per call site.
/// * **`detach` makes the command a `switch --detach` and disables path restoration**, which is
///   how a caller says "I really did mean the commit, not the file".
/// * **`path` restores from the index by default and from `--source` when given**, and it never
///   moves `HEAD` — the same operation `rivet restore` performs, reachable under the name
///   muscle memory expects.
/// * **`force` and `ours` are both destructive and different.** `-f` discards local changes to
///   make the target tree match; `--ours` resolves conflicts in favour of the current branch
///   *during* a merge, and is only meaningful then. Applying `--ours` outside a merge is an
///   error in Git, not a no-op.
/// * **A detached `HEAD` is a warning, not a refusal**, and the warning goes to stderr so that a
///   script capturing stdout is unaffected. `rivet checkout abc1234` must succeed with the
///   warning, exactly as `git checkout` does, or every CI script that checks out a sha breaks.
/// * **`orphan` starts a new history with no parents**, which is not "a branch at the current
///   commit" — it is an empty tree and an unborn `HEAD`, and the first commit on it is a root
///   commit. Confusing the two produces a branch that shares the entire history instead of none
///   of it.
#[derive(Debug, clap::Args)]
pub struct CheckoutArgs {
    /// The branch, commit or paths to check out.
    #[arg(value_name = "TREE-OR-PATH")]
    pub target: Option<String>,

    /// The paths to restore from `--source` or the index.
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<PathBuf>,

    /// Start a new branch with no history at this commit. Not implemented in v1.
    #[arg(long = "orphan", value_name = "BRANCH")]
    pub orphan: Option<String>,

    /// Create the branch before checking it out.
    #[arg(short = 'b', long = "branch", value_name = "BRANCH")]
    pub create: Option<String>,

    /// Create the branch, overwriting an existing one.
    #[arg(short = 'B', value_name = "BRANCH", requires = "target")]
    pub force_create: Option<String>,

    /// Check out the target commit with a detached `HEAD` rather than a branch.
    #[arg(short = 'd', long = "detach")]
    pub detach: bool,

    /// Add this many commits to the branch before checking it out.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// On a conflicted checkout, keep the current branch's version.
    #[arg(long = "ours")]
    pub ours: bool,

    /// On a conflicted checkout, keep the incoming version.
    #[arg(long = "theirs")]
    pub theirs: bool,

    /// Throw away local modifications and untracked files obstructing the checkout.
    #[arg(long = "overwrite-ignore")]
    pub overwrite_ignore: bool,

    /// Merge local modifications into the checked-out tree instead of refusing.
    #[arg(short = 'm', long = "merge")]
    pub merge: bool,

    /// Check out from this commit rather than the index.
    #[arg(long = "source", value_name = "COMMIT")]
    pub source: Option<String>,

    /// Track the remote branch that the target is a remote-tracking ref of.
    #[arg(short = 't', long = "track")]
    pub track: bool,

    /// Show what would happen, and change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Ignore pathspecs that match nothing instead of failing.
    #[arg(long = "ignore-skip-worktree-bits")]
    pub ignore_skip_worktree_bits: bool,
}

/// Arguments to `rivet merge`.
///
/// # Caveats
///
/// * **`abort`, `continue` and `quit` are not merges.** They are operations on the state a
///   previous merge left behind, they are mutually exclusive with each other and with the
///   positional, and they must be resolved *before* anything else — a handler that looked at
///   `ours` first would try to merge a commit named "abort".
/// * **`abort` is `reset --hard ORIG_HEAD` plus clearing the merge state**, and both halves are
///   required. Clearing `MERGE_HEAD` without restoring the work tree leaves the conflict markers
///   in place with no state saying they are conflicts; restoring the tree without clearing the
///   state leaves a repository that thinks a merge is in progress forever.
/// * **`continue` commits with the recorded message and parents.** It reads `MERGE_HEAD` and
///   `MERGE_MSG` and refuses if any path is still unmerged, which is the check that makes
///   `rivet add <file> && rivet merge --continue` the resolution workflow.
/// * **`strategy` is accepted and only `ort`-like behaviour exists.** `ours` is the one strategy
///   worth implementing (it records a merge commit whose tree is ours, discarding theirs);
///   `recursive`, `octopus` and `subtree` must fail loudly rather than silently behaving like
///   the default.
/// * **`no_ff` and `ff_only` are opposites and the grammar must not let both be set.** The
///   default is fast-forward when possible; `--no-ff` forces a merge commit even then, and
///   `--ff-only` refuses to merge at all when it is not possible.
/// * **`squash` does not create a merge commit at all.** It stages the merge result and stops,
///   leaving the index holding the merged tree and `HEAD` unchanged, for a single later commit.
///   A squash implemented as a normal merge produces a two-parent commit the user did not ask
///   for.
/// * **The pre-merge safety check is `checkout`'s check**, per plan §4 Phase 9: the set of paths
///   differing between `HEAD` and the merge result is computed first, and the merge refuses when
///   any of them has a work tree change or an index change that the merge would overwrite. That
///   includes the modify-in-work-tree / delete-in-merge case the plan calls out.
/// * **Conflicts are written to the index as stages 1/2/3 and to
///   `.git/MERGE_HEAD`/`MERGE_MSG`**, exactly as Git writes them, so that `git status`,
///   `git mergetool` and `git merge --abort` all work on Rivet's in-progress merge. Recording
///   the conflicts anywhere else makes the repository unusable from Git until it is finished.
/// * **A merge that conflicts exits `1`, not `128`.** It is the requested operation reporting a
///   partial success — the merge is *in progress* and the user is expected to resolve it — and a
///   script that treats `1` as "the command did not run" would try again on top of a conflicted
///   index.
#[derive(Debug, clap::Args)]
pub struct MergeArgs {
    /// The commit to merge into the current branch.
    #[arg(value_name = "COMMIT")]
    pub commit: Option<String>,

    /// Merge several commits at once, as an octopus.
    #[arg(value_name = "COMMIT")]
    pub additional: Vec<String>,

    /// Abort the merge in progress and restore the pre-merge state.
    #[arg(long = "abort")]
    pub abort: bool,

    /// Complete the merge in progress, resolving the recorded conflicts.
    #[arg(long = "continue")]
    pub continue_merge: bool,

    /// Abandon the merge in progress without committing anything.
    #[arg(long = "quit")]
    pub quit: bool,

    /// Force a merge commit even when a fast-forward is possible.
    #[arg(long = "no-ff")]
    pub no_ff: bool,

    /// Refuse to merge when a fast-forward is not possible.
    #[arg(long = "ff-only")]
    pub ff_only: bool,

    /// Stage the merge result and stop, leaving `HEAD` where it is.
    #[arg(long = "squash")]
    pub squash: bool,

    /// Refuse when the merge would not be a fast-forward, committing nothing.
    #[arg(long = "ff", value_name = "WHEN")]
    pub ff: Option<Option<String>>,

    /// The merge strategy to use.
    #[arg(short = 's', long = "strategy", value_name = "NAME")]
    pub strategy: Option<String>,

    /// A strategy-specific option.
    #[arg(short = 'X', long = "strategy-option", value_name = "OPTION")]
    pub strategy_option: Vec<String>,

    /// How conflicting hunks are marked in the work tree.
    #[arg(long = "conflict", value_name = "STYLE", value_enum)]
    pub conflict: Option<ConflictStyleArg>,

    /// The message for the merge commit.
    #[arg(short = 'm', long = "message", value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Do not commit the merge even when it succeeds cleanly.
    #[arg(long = "no-commit")]
    pub no_commit: bool,

    /// Skip the pre-merge and post-merge hooks. They are not implemented in v1 (plan §1).
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Refuse to merge a commit whose signature cannot be verified.
    #[arg(long = "verify-signatures")]
    pub verify_signatures: bool,

    /// Show or suppress the diffstat of the merge result.
    #[arg(long = "stat", value_name = "MODE")]
    pub stat: Option<Option<String>>,

    /// Suppress the diffstat of the merge result.
    #[arg(long = "no-stat")]
    pub no_stat: bool,

    /// Allow a merge between two commits that share no ancestor.
    #[arg(long = "allow-unrelated-histories")]
    pub allow_unrelated_histories: bool,
}

/// Phase 6 — list, create, rename and delete branches.
///
/// # Purpose
///
/// The one command for every operation on `refs/heads/*`: the listing form that shows the
/// current branch, the form that creates a branch at a start point, the rename, the copy and the
/// two deletes.
///
/// # Exit codes
///
/// * `0` — the branch was created, renamed, copied, deleted or listed.
/// * `1` — `--list <pattern>` matched no branch. The requested negative, matching `git branch`.
/// * `128` — the name is not a legal ref name, the branch already exists, the start point does
///   not resolve, the branch does not exist, or `-d` refused because it is unmerged.
///
/// # Caveats
///
/// * **`-d` refuses to delete the current branch and `-D` does not.** Actually deleting the
///   branch `HEAD` is attached to leaves `HEAD` pointing at a ref that does not exist, which is
///   an unborn `HEAD` on a branch the user never created; both flags must refuse it, and Git's
///   message names the branch.
/// * **The "is it merged" test for `-d` compares against the current branch *or* the upstream**,
///   and GitHub's default workflow makes those differ. Comparing only against `HEAD` refuses to
///   delete a branch that is fully pushed, which is the most common use of `-d` there is.
/// * **A rename updates `HEAD` when the renamed branch is the current one**, and it must —
///   `HEAD` is a symbolic ref to `refs/heads/<old>` and a rename that moved the ref without
///   rewriting `HEAD` leaves a detached-looking repository.
/// * **A rename also moves the reflog**, and `branch.<old>.remote` / `branch.<old>.merge` are
///   *not* renamed by Git. That asymmetry is deliberate in Git and reproducing it matters more
///   than tidying it up.
/// * **`--show-current` prints nothing and exits `0` on a detached `HEAD`**, where `--list` with
///   no pattern prints every branch. The two are different questions and neither is the fallback
///   for the other.
/// * **The listing marks the current branch with a leading `*` and colours it**, and the marker
///   column is two characters wide even when empty. The differential tests compare this output,
///   so the column widths are interface.
pub fn run_branch(global: &GlobalArgs, args: &BranchArgs) -> Result<i32> {
    todo!()
}

/// Phase 6 — switch to a branch.
///
/// # Purpose
///
/// Move `HEAD` onto a branch, or create one and move onto it, updating the index and the work
/// tree to match — the branch-only subset of `checkout`, with a commit id refused by design.
///
/// # Exit codes
///
/// * `0` — the switch happened.
/// * `128` — the branch does not exist (and `--create` was not given), the target is a commit
///   rather than a branch, `--create` names an existing branch without `-C`, a local
///   modification would be overwritten, or the checkout could not be completed.
///
/// # Caveats
///
/// * **The safety check runs before anything is written.** The plan's `victim.txt` scenario —
///   a file modified in the work tree and deleted in the target — must refuse cleanly, and the
///   check is the same one `checkout` performs; it must not be reimplemented here with a
///   narrower condition.
/// * **A failed switch must leave the repository exactly as it was.** The two-phase apply —
///   validate every path, then write — is what makes that true; applying as it goes leaves a
///   work tree that matches neither branch and a `HEAD` that says one thing while the files say
///   another. The `--dry-run` form must exercise the same validation so that it predicts the
///   real run.
/// * **`--merge` records conflict stages in the index and leaves the merge in progress**, which
///   is the same state `merge` produces, not a broken switch. A `--merge` that left no state
///   behind would make the conflicts unresolvable through `rivet add`.
/// * **A switch to the branch already checked out is a success that prints nothing**, except
///   when `--discard-changes` was given, where it resets the work tree. Those two behaviours
///   are the same command line with one flag and must not share a code path.
/// * **The reflog entry says `checkout: moving from <old> to <new>`**, and its wording is what
///   makes a reflog searchable. `switch` writes the same message Git's `checkout` does, because
///   a user reading `git reflog` cannot tell which binary wrote it and should not have to.
pub fn run_switch(global: &GlobalArgs, args: &SwitchArgs) -> Result<i32> {
    todo!()
}

/// Phase 6 — check out a branch, a commit, or paths from one.
///
/// # Purpose
///
/// The general form: move `HEAD` to a branch or a commit, or restore named paths from the index
/// or a tree, deciding which of the two from the arguments.
///
/// # Exit codes
///
/// * `0` — the checkout or the path restoration happened.
/// * `128` — the target does not resolve, a pathspec matches nothing, a local modification would
///   be overwritten, `--ours` was given outside a merge, or the tree could not be written.
///
/// # Caveats
///
/// * **The branch-versus-path disambiguation is the core of this command**, per
///   [`CheckoutArgs`]. It must be resolved once, with the same rules Git uses — `--`,
///   `refs/`-prefixed names, and a name matching an existing branch — because a wrong answer
///   either detaches the user's `HEAD` or overwrites a file they were editing.
/// * **Detaching prints a warning to stderr and succeeds.** The warning names the commit and
///   says how to undo it; suppressing it makes a script-visible difference only in that the
///   user never learns they detached.
/// * **The checkout is two-phase, and the first phase is the whole safety story.** Every path
///   that would be written is checked against the work tree and the index before a single byte
///   is written; the plan's Phase 4 "Done when" makes the `victim.txt` refusal a regression test,
///   so the check is required, not advisory.
/// * **`--force` skips the safety check for tracked files but not for untracked ones.** A file
///   that is untracked in both the current and the target tree is not the checkout's to
///   clobber; Git only overwrites it with `--force` *and* only when it obstructs a path in the
///   target, and that pairing is the difference between a useful
///   [`rivet_core::Error::UntrackedOverwrite`] and silent data loss.
/// * **A symlink is written as a symlink and a gitlink is left alone.** Checking out a
///   submodule's gitlink must not descend into it or create a directory where a checkout of the
///   submodule belongs; the entry is recorded in the index and the work tree content is the
///   submodule's own business.
/// * **Case-insensitive filesystems must be detected before the collision check.** On macOS and
///   Windows two tree entries differing only in case cannot both be written, and the failure
///   must be reported as a collision naming both paths rather than as an I/O error on the
///   second write. See `rivet_core::worktree::paths_collide`.
pub fn run_checkout(global: &GlobalArgs, args: &CheckoutArgs) -> Result<i32> {
    todo!()
}

/// Phase 9 — join development histories together.
///
/// # Purpose
///
/// Merge a commit into the current branch: fast-forward when the merge base allows it, otherwise
/// a three-way merge of the two trees with content merging per file, conflicts written to the
/// index as stages 1/2/3 and to the work tree with markers, and the merge commit recorded —
/// unless it conflicted, in which case the state is left for the user to resolve.
///
/// # Exit codes
///
/// * `0` — the merge completed: a fast-forward, or a clean merge commit.
/// * `1` — the merge left conflicts. **The merge is in progress**, not failed; the user runs
///   `rivet add` and `rivet merge --continue`, or `rivet merge --abort`.
/// * `128` — a merge is already in progress, there is no merge in progress for
///   `--continue`/`--abort`/`--quit`, the commit does not resolve, the histories are unrelated
///   and `--allow-unrelated-histories` was not given, or a local modification would be
///   overwritten by the pre-merge safety check.
///
/// # Caveats
///
/// * **A fast-forward is not a merge commit, and `--no-ff` is the only thing that forces one.**
///   The distinction is recorded in the history for ever, so a handler must decide it from the
///   merge base and the options and not from whether the trees happened to differ.
/// * **The merge base is a set, and a criss-cross produces more than one.** Rivet picks one
///   base rather than building a recursive virtual base (plan §4 Phase 9 says so explicitly),
///   and the consequence is that a criss-cross merge can produce a *different but valid* result
///   from `git merge`. That must be stated in the output or in the docs, not discovered by a
///   user comparing two repositories.
/// * **Conflicts are recorded in three places and all three are required**:
///   `.git/MERGE_HEAD` with the parents, `.git/MERGE_MSG` with the message `--continue` will
///   use, and the index's stages 1/2/3. Writing any two of the three produces a repository that
///   `git` cannot finish the merge in.
/// * **The pre-merge safety check is the same one `checkout` performs**, and it must run before
///   `MERGE_HEAD` is written. A merge that refuses after recording its state leaves the
///   repository claiming a merge is in progress when nothing was merged.
/// * **`--abort` restores `ORIG_HEAD` and clears the state**, and it must do both and in that
///   order: a hard reset that failed partway with the merge state already cleared is a
///   repository with conflict markers and no way to abort.
/// * **A merge whose result equals `HEAD`'s tree with no `--no-ff` is "Already up to date"**,
///   which is a success that prints one line. It is not an error, and a script depends on the
///   exit code being `0` so that a `pull` loop does not report a failure on every no-op.
/// * **Submodules and `.gitattributes`-driven merge drivers are not implemented** (plan §1), and
///   a merge that encounters a `.gitattributes` naming a merge driver must say so rather than
///   silently falling back to a text merge the user did not configure.
pub fn run_merge(global: &GlobalArgs, args: &MergeArgs) -> Result<i32> {
    todo!()
}
