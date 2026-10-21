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
    if args.format.is_some() {
        anyhow::bail!("branch: --format is not implemented");
    }
    let mut repository = super::open(global)?;
    let head = repository.head()?;

    if args.show_current {
        if let Some(branch) = head.branch() {
            println!("{}", branch.short_name());
        }
        return Ok(0);
    }

    if let Some(upstream) = &args.set_upstream {
        let branch = match &args.name {
            Some(name) => RefName::branch(BStr::new(name.as_bytes()))?,
            None => head
                .branch()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("could not set upstream of HEAD to {upstream} when it does not point to any branch"))?,
        };
        if !repository.refs().exists(&branch) {
            anyhow::bail!("branch '{}' does not exist", branch.short_name());
        }
        let target = resolve_upstream(&repository, upstream)?;
        let message = set_upstream(&mut repository, &branch, &target)?;
        println!("{message}");
        return Ok(0);
    }

    if args.delete || args.force_delete {
        let force = args.force_delete || args.force;
        let names: Vec<&String> = args.name.iter().chain(args.start_point.iter()).collect();
        let mut code = 0;
        for name in names {
            code = code.max(delete_branch(&repository, &head, name, force, args.remotes)?);
        }
        return Ok(code);
    }

    if args.rename || args.force_rename || args.copy || args.force_copy {
        let force = args.force_rename || args.force_copy || args.force;
        let copy = args.copy || args.force_copy;
        let (old, new) = match (&args.name, &args.start_point) {
            (Some(old), Some(new)) => (RefName::branch(BStr::new(old.as_bytes()))?, new.clone()),
            (Some(new), None) => (
                head.branch()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("cannot rename the current branch while not on any"))?,
                new.clone(),
            ),
            _ => anyhow::bail!("branch name required"),
        };
        let new_ref = valid_branch_name(&new)?;
        return rename_branch(&mut repository, &head, &old, &new_ref, force, copy);
    }

    let listing = args.list.is_some()
        || args.all
        || args.remotes
        || args.merged.is_some()
        || args.no_merged.is_some()
        || args.points_at.is_some()
        || args.name.is_none();
    if !listing {
        let name = args.name.as_deref().unwrap_or_default();
        let start = args.start_point.as_deref().unwrap_or("HEAD");
        let messages = create_branch(&mut repository, &head, name, start, args.force, !args.no_track)?;
        for message in messages {
            println!("{message}");
        }
        return Ok(0);
    }

    // Listing. A pattern can come from `--list <pattern>` or, with `--list`, a positional.
    let pattern: Option<String> = match &args.list {
        Some(Some(pattern)) => Some(pattern.clone()),
        _ => args.name.clone(),
    };
    let merged_filter = match &args.merged {
        Some(spec) => Some((true, super::resolve_commit(&repository, spec.as_deref().unwrap_or("HEAD"))?)),
        None => match &args.no_merged {
            Some(spec) => Some((false, super::resolve_commit(&repository, spec.as_deref().unwrap_or("HEAD"))?)),
            None => None,
        },
    };
    let points_at = match &args.points_at {
        Some(spec) => Some(super::resolve_rev(&repository, spec)?),
        None => None,
    };

    struct Row {
        marker: char,
        label: String,
        oid: Option<ObjectId>,
        symref: Option<String>,
    }
    let mut rows: Vec<Row> = Vec::new();
    if let HeadState::Detached { oid } = &head
        && !args.remotes {
            rows.push(Row {
                marker: '*',
                label: format!("(HEAD detached at {})", super::short_oid(oid)),
                oid: Some(*oid),
                symref: None,
            });
        }
    if !args.remotes {
        for (name, oid) in repository.refs().list_branches()? {
            let current = head.branch() == Some(&name);
            rows.push(Row {
                marker: if current { '*' } else { ' ' },
                label: name.short_name().to_string(),
                oid: Some(oid),
                symref: None,
            });
        }
    }
    if args.remotes || args.all {
        for (name, target) in repository.refs().list(b"refs/remotes/")? {
            let short = String::from_utf8_lossy(&name.as_bytes()[b"refs/remotes/".len()..]).into_owned();
            let label = if args.all { format!("remotes/{short}") } else { short };
            match target {
                rivet_core::RefTarget::Symbolic(to) => {
                    let to_short = String::from_utf8_lossy(
                        to.as_bytes().strip_prefix(b"refs/remotes/").unwrap_or(to.as_bytes()),
                    )
                    .into_owned();
                    let oid = repository.refs().resolve_oid(&to).ok();
                    rows.push(Row { marker: ' ', label, oid, symref: Some(to_short) });
                }
                rivet_core::RefTarget::Direct(oid) => {
                    rows.push(Row { marker: ' ', label, oid: Some(oid), symref: None });
                }
            }
        }
    }

    let mut kept = Vec::new();
    for row in rows {
        if let Some(pattern) = &pattern
            && !glob_match(pattern.as_bytes(), row.label.as_bytes()) && !row.label.starts_with('(') {
                continue;
            }
        if let Some((want_merged, target)) = merged_filter {
            let Some(oid) = row.oid else { continue };
            if repository.is_ancestor(oid, target)? != want_merged {
                continue;
            }
        }
        if let Some(points_at) = points_at
            && row.oid != Some(points_at) {
                continue;
            }
        kept.push(row);
    }

    let verbose = global.verbose && !global.quiet;
    let width = kept.iter().filter(|r| r.symref.is_none()).map(|r| r.label.chars().count()).max().unwrap_or(0);
    for row in &kept {
        match (&row.symref, verbose) {
            (Some(to), _) => println!("{} {} -> {}", row.marker, row.label, to),
            (None, true) => {
                let oid = row.oid.expect("listed branches have ids");
                let subject = super::commit_subject(&repository, &oid)?;
                println!(
                    "{} {:<width$} {} {}",
                    row.marker,
                    row.label,
                    super::short_oid(&oid),
                    subject
                );
            }
            (None, false) => println!("{} {}", row.marker, row.label),
        }
    }
    Ok(0)
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
    if args.merge || args.conflict.is_some() {
        anyhow::bail!("switch: --merge is not implemented");
    }
    if args.dry_run {
        anyhow::bail!("switch: --dry-run is not implemented");
    }
    let mut repository = super::open(global)?;

    if let Some(orphan) = &args.orphan {
        return orphan_branch(&mut repository, orphan, global.quiet);
    }

    if let Some(new) = args.create.as_ref().or(args.force_create.as_ref()) {
        let start = args
            .start_point
            .as_deref()
            .or(args.branch.as_deref())
            .unwrap_or("HEAD");
        return checkout_new_branch(
            &mut repository,
            new,
            start,
            args.force_create.is_some(),
            args.force,
            !args.no_track,
            global.quiet,
        );
    }

    let Some(target) = &args.branch else {
        anyhow::bail!("missing branch or commit argument");
    };
    let target = resolve_previous(&repository, target)?;

    if args.detach {
        let oid = super::resolve_commit(&repository, &target)?;
        return switch_to(&mut repository, Destination::Detached(oid), &target, args.force, global.quiet, false);
    }

    let branch = RefName::branch(BStr::new(target.as_bytes())).ok();
    if let Some(branch) = branch.filter(|b| repository.refs().exists(b)) {
        return switch_to(&mut repository, Destination::Branch(branch), &target, args.force, global.quiet, false);
    }
    if let Some(remote_ref) = guess_remote_branch(&repository, &target)? {
        let start = String::from_utf8_lossy(remote_ref.as_bytes()).into_owned();
        return checkout_new_branch(&mut repository, &target, &start, false, args.force, true, global.quiet);
    }
    if super::resolve_commit(&repository, &target).is_ok() {
        let what = if repository.refs().exists(&RefName::tag(BStr::new(target.as_bytes()))?) {
            "tag"
        } else {
            "commit"
        };
        anyhow::bail!("a branch is expected, got {what} '{target}'");
    }
    anyhow::bail!("invalid reference: {target}")
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
    if args.merge {
        anyhow::bail!("checkout: --merge is not implemented");
    }
    if args.ours || args.theirs {
        anyhow::bail!("checkout: --ours/--theirs are not implemented");
    }
    if args.dry_run {
        anyhow::bail!("checkout: --dry-run is not implemented");
    }
    let mut repository = super::open(global)?;

    if let Some(orphan) = &args.orphan {
        return orphan_branch(&mut repository, orphan, global.quiet);
    }

    if let Some(new) = args.create.as_ref().or(args.force_create.as_ref()) {
        let start = args.target.as_deref().unwrap_or("HEAD");
        return checkout_new_branch(
            &mut repository,
            new,
            start,
            args.force_create.is_some(),
            args.force,
            args.track || true,
            global.quiet,
        );
    }

    let Some(first) = &args.target else {
        // `git checkout` with no arguments only reports; there is nothing to switch to.
        return Ok(0);
    };

    // Branch-or-path disambiguation: the first word is a revision if it resolves to one,
    // otherwise every word is a path. `--source` always means paths.
    let first_rev = if args.source.is_some() {
        None
    } else {
        let resolved = resolve_previous(&repository, first)?;
        if super::resolve_rev(&repository, &resolved).is_ok() {
            Some(resolved)
        } else {
            None
        }
    };

    let path_mode = !args.paths.is_empty() || first_rev.is_none() || args.source.is_some();
    if path_mode && (first_rev.is_some() || args.source.is_some() || first_rev.is_none()) {
        let (source_spec, path_args): (Option<String>, Vec<PathBuf>) = match (&args.source, &first_rev) {
            (Some(source), _) => {
                let mut paths = vec![PathBuf::from(first)];
                paths.extend(args.paths.iter().cloned());
                (Some(source.clone()), paths)
            }
            (None, Some(rev)) if !args.paths.is_empty() => (Some(rev.clone()), args.paths.clone()),
            (None, Some(_)) => (None, Vec::new()),
            (None, None) => {
                // A name that is neither a revision nor a tracked path may be a remote branch
                // to create locally (`git checkout feature` with only `origin/feature`).
                if args.paths.is_empty()
                    && let Some(remote_ref) = guess_remote_branch(&repository, first)? {
                        let start = String::from_utf8_lossy(remote_ref.as_bytes()).into_owned();
                        return checkout_new_branch(&mut repository, first, &start, false, args.force, true, global.quiet);
                    }
                let mut paths = vec![PathBuf::from(first)];
                paths.extend(args.paths.iter().cloned());
                (None, paths)
            }
        };
        if !path_args.is_empty() {
            // Git counts restored paths only when the command line had no `--`; clap consumes
            // the separator, so the raw arguments are the only place it is still visible.
            let dash_dash = std::env::args().any(|a| a == "--");
            return checkout_paths(&mut repository, source_spec.as_deref(), &path_args, global.quiet || dash_dash);
        }
    }

    let target = first_rev.expect("a revision was resolved");
    let branch = RefName::branch(BStr::new(target.as_bytes())).ok();
    if let Some(branch) = branch.filter(|b| !args.detach && repository.refs().exists(b)) {
        return switch_to(&mut repository, Destination::Branch(branch), &target, args.force, global.quiet, false);
    }
    let oid = super::resolve_commit(&repository, &target)?;
    switch_to(&mut repository, Destination::Detached(oid), &target, args.force, global.quiet, !args.detach)
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
    let mut repository = super::open(global)?;
    let git_dir = repository.git_dir().to_path_buf();

    if args.abort {
        if !rivet_core::merge_in_progress(&git_dir) {
            anyhow::bail!("There is no merge to abort (MERGE_HEAD missing).");
        }
        let worktree = repository.worktree()?.clone();
        let odb_format = repository.format();
        let _ = odb_format;
        let mut index = std::mem::take(repository.index_mut());
        let result = rivet_core::abort_merge(&worktree, &mut index, repository.odb(), &git_dir);
        *repository.index_mut() = index;
        result?;
        // `merge --abort` is `reset --merge`, and Git logs it as such.
        if let Some(head) = repository.head()?.oid() {
            let who = repository.committer_signature()?;
            let entry = rivet_core::ReflogEntry::new(head, head, who, BString::from("reset: moving to HEAD"));
            repository.refs().append_reflog(&RefName::head(), &entry)?;
        }
        // `abort_merge` restores files and the index; `HEAD` itself never moved during a
        // conflicted merge, so there is nothing else to undo.
        return Ok(0);
    }
    if args.quit {
        rivet_core::clear_merge_state(&git_dir)?;
        return Ok(0);
    }
    if args.continue_merge {
        if !rivet_core::merge_in_progress(&git_dir) {
            anyhow::bail!("There is no merge in progress (MERGE_HEAD missing).");
        }
        if repository.index().has_conflicts() {
            eprintln!("error: Committing is not possible because you have unmerged files.");
            eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
            eprintln!("hint: as appropriate to mark resolution and make a commit.");
            eprintln!("fatal: Exiting because of an unresolved conflict.");
            return Ok(128);
        }
        let message = std::fs::read(git_dir.join("MERGE_MSG")).unwrap_or_default();
        let message = strip_comment_lines(&message);
        let options = rivet_core::repo::CommitOptions::new(BStr::new(&message));
        let oid = repository.commit(&options)?;
        let branch = super::head_reflog_label(&repository)?;
        println!("[{branch} {}] {}", super::short_oid(&oid), super::commit_subject(&repository, &oid)?);
        return Ok(0);
    }

    if args.squash {
        anyhow::bail!("merge: --squash is not implemented");
    }
    if args.verify_signatures {
        anyhow::bail!("merge: --verify-signatures is not implemented");
    }
    if let Some(strategy) = &args.strategy
        && strategy != "ort" && strategy != "recursive" {
            anyhow::bail!("merge: strategy '{strategy}' is not implemented");
        }
    if !args.strategy_option.is_empty() {
        anyhow::bail!("merge: -X strategy options are not implemented");
    }
    if !args.additional.is_empty() {
        anyhow::bail!("merge: octopus merges are not implemented");
    }
    let Some(spec) = &args.commit else {
        anyhow::bail!("No remote for the current branch.");
    };
    if rivet_core::merge_in_progress(&git_dir) {
        eprintln!("error: Merging is not possible because you have unmerged files.");
        eprintln!("fatal: You have not concluded your merge (MERGE_HEAD exists).");
        return Ok(128);
    }
    if repository.index().has_conflicts() {
        eprintln!("error: Merging is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        eprintln!("fatal: Exiting because of an unresolved conflict.");
        return Ok(128);
    }

    let spec = resolve_previous(&repository, spec)?;
    let theirs = super::resolve_commit(&repository, &spec)
        .map_err(|_| anyhow::anyhow!("{spec} - not something we can merge"))?;
    let ff = match args.ff.as_ref().map(|v| v.as_deref()) {
        Some(Some("only")) => FastForward::Only,
        Some(Some("false")) | Some(Some("no")) => FastForward::Never,
        _ if args.ff_only => FastForward::Only,
        _ if args.no_ff => FastForward::Never,
        _ => FastForward::Allow,
    };
    let message = args.message.clone().unwrap_or_else(|| merge_message(&repository, &spec));
    let mut options = rivet_core::MergeOptions::new();
    options.find_renames = super::renames_enabled(&repository, "merge.renames");
    if let Some(style) = args.conflict {
        options.conflict_style = style.into();
    }
    let show_stat = !args.no_stat && !global.quiet;
    merge_commit(
        &mut repository,
        theirs,
        &spec,
        &message,
        ff,
        args.no_commit,
        args.allow_unrelated_histories,
        &options,
        show_stat,
        "merge",
        args.message.is_some(),
    )
}

use bstr::{BStr, BString, ByteSlice};
use rivet_core::ops::HeadTarget;
use rivet_core::{HeadState, ObjectId, RefName, Repository};

/// Whether `merge` may fast-forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastForward {
    /// Fast-forward when possible, merge otherwise (the default).
    Allow,
    /// Always create a merge commit (`--no-ff`).
    Never,
    /// Fast-forward or fail (`--ff-only`).
    Only,
}

/// Where a switch is going.
enum Destination {
    Branch(RefName),
    Detached(ObjectId),
}

/// `-` and `@{-1}` name the branch (or commit) checked out before the current one.
fn resolve_previous(repository: &Repository, spec: &str) -> Result<String> {
    let n = if spec == "-" {
        1
    } else if let Some(rest) = spec.strip_prefix("@{-").and_then(|r| r.strip_suffix('}')) {
        rest.parse::<usize>().unwrap_or(0)
    } else {
        return Ok(spec.to_string());
    };
    let log = repository.refs().reflog(&RefName::head())?;
    let mut seen = 0;
    for entry in log.iter().rev() {
        let Some(rest) = entry.message.strip_prefix(b"checkout: moving from ") else { continue };
        seen += 1;
        if seen == n {
            let from = rest.split_str(" to ").next().unwrap_or_default();
            return Ok(String::from_utf8_lossy(from).into_owned());
        }
    }
    anyhow::bail!("invalid reference: {spec}")
}

fn valid_branch_name(name: &str) -> Result<RefName> {
    // `main` prints the message after `fatal: `; the hints follow on their own lines.
    let refuse = || {
        anyhow::anyhow!(
            "'{name}' is not a valid branch name\nhint: See 'git help check-ref-format'\n\
             hint: Disable this message with \"git config set advice.refSyntax false\""
        )
    };
    if name == "HEAD" || name.starts_with('-') {
        return Err(refuse());
    }
    RefName::branch(BStr::new(name.as_bytes())).map_err(|_| refuse())
}

/// The start-point name the reflog records: Git spells an attached `HEAD` as its branch.
fn start_label(repository: &Repository, start: &str) -> Result<String> {
    if start == "HEAD"
        && let HeadState::Attached { branch, .. } = repository.head()? {
            return Ok(branch.short_name().to_string());
        }
    Ok(start.to_string())
}

/// A remote-tracking ref `refs/remotes/<remote>/<name>`, if exactly one remote has it.
fn guess_remote_branch(repository: &Repository, name: &str) -> Result<Option<RefName>> {
    let mut found = Vec::new();
    for (remote_ref, _) in repository.refs().list_remote()? {
        let short = &remote_ref.as_bytes()[b"refs/remotes/".len()..];
        if let Some(slash) = short.find_byte(b'/')
            && &short[slash + 1..] == name.as_bytes() {
                found.push(remote_ref.clone());
            }
    }
    Ok(if found.len() == 1 { found.pop() } else { None })
}

/// The ref an upstream spec names: a remote-tracking branch, or a local one.
fn resolve_upstream(repository: &Repository, spec: &str) -> Result<RefName> {
    for candidate in [format!("refs/remotes/{spec}"), format!("refs/heads/{spec}"), spec.to_string()] {
        if let Ok(name) = RefName::from_bytes(candidate.as_bytes())
            && repository.refs().exists(&name) && (name.is_remote() || name.is_branch()) {
                return Ok(name);
            }
    }
    anyhow::bail!("the requested upstream branch '{spec}' does not exist")
}

/// Record `upstream` as `branch`'s upstream in the config, returning Git's confirmation line.
pub fn set_upstream(repository: &mut Repository, branch: &RefName, upstream: &RefName) -> Result<String> {
    let (remote, merge, shown) = if upstream.is_remote() {
        let short = &upstream.as_bytes()[b"refs/remotes/".len()..];
        let slash = short.find_byte(b'/').unwrap_or(short.len());
        let remote = BString::from(&short[..slash]);
        let rest = short.get(slash + 1..).unwrap_or_default();
        // The merge key is the branch's name on the remote, found through the fetch refspec
        // in the common case of the default mapping.
        let mut merge = BString::from("refs/heads/");
        merge.extend_from_slice(rest);
        (remote, merge, String::from_utf8_lossy(short).into_owned())
    } else {
        (
            BString::from("."),
            BString::from(upstream.as_bytes()),
            upstream.short_name().to_string(),
        )
    };
    let short = branch.short_name().to_string();
    let config = repository.config_mut();
    config.set(BStr::new(format!("branch.{short}.remote").as_bytes()), &remote)?;
    config.set(BStr::new(format!("branch.{short}.merge").as_bytes()), &merge)?;
    repository.write_config()?;
    Ok(format!("branch '{short}' set up to track '{shown}'."))
}

/// Create a branch; returns the lines Git prints (the tracking confirmation, if any).
fn create_branch(
    repository: &mut Repository,
    head: &HeadState,
    name: &str,
    start: &str,
    force: bool,
    track: bool,
) -> Result<Vec<String>> {
    let branch = valid_branch_name(name)?;
    let exists = repository.refs().exists(&branch);
    if exists && !force {
        anyhow::bail!("a branch named '{name}' already exists");
    }
    if exists && head.branch() == Some(&branch) {
        anyhow::bail!(
            "cannot force update the branch '{name}' used by worktree at '{}'",
            repository.worktree_path().map(|p| p.display().to_string()).unwrap_or_default()
        );
    }
    let start_oid = super::resolve_commit(repository, start)
        .map_err(|_| anyhow::anyhow!("not a valid object name: '{start}'"))?;
    let who = repository.committer_signature()?;
    let label = start_label(repository, start)?;
    let reason = if exists {
        format!("branch: Reset to {label}")
    } else {
        format!("branch: Created from {label}")
    };
    repository.set_ref_logged(&branch, start_oid, &who, BStr::new(reason.as_bytes()))?;

    let mut messages = Vec::new();
    if track {
        let upstream = RefName::from_bytes(format!("refs/remotes/{start}").as_bytes())
            .ok()
            .filter(|r| repository.refs().exists(r));
        if let Some(upstream) = upstream {
            messages.push(set_upstream(repository, &branch, &upstream)?);
        }
    }
    Ok(messages)
}

fn delete_branch(repository: &Repository, head: &HeadState, name: &str, force: bool, remote: bool) -> Result<i32> {
    let reference = if remote {
        RefName::from_bytes(format!("refs/remotes/{name}").as_bytes())?
    } else {
        RefName::branch(BStr::new(name.as_bytes()))?
    };
    let Some(oid) = repository.refs().try_read(&reference)?.and_then(|t| t.oid().ok()) else {
        let what = if remote { "remote-tracking branch" } else { "branch" };
        eprintln!("error: {what} '{name}' not found");
        return Ok(1);
    };
    if !remote && head.branch() == Some(&reference) {
        eprintln!(
            "error: cannot delete branch '{name}' used by worktree at '{}'",
            repository.worktree_path().map(|p| p.display().to_string()).unwrap_or_default()
        );
        return Ok(1);
    }
    if !force && !remote {
        // Merged into its upstream if it has one, otherwise into HEAD.
        let upstream = branch_upstream(repository, name)?;
        let against = match upstream.as_ref().and_then(|u| repository.refs().resolve_oid(u).ok()) {
            Some(oid) => Some(oid),
            None => head.oid(),
        };
        let merged = match against {
            Some(target) => repository.is_ancestor(oid, target)?,
            None => false,
        };
        if !merged {
            eprintln!("error: the branch '{name}' is not fully merged");
            eprintln!("hint: If you are sure you want to delete it, run 'git branch -D {name}'");
            eprintln!("hint: Disable this message with \"git config set advice.forceDeleteBranch false\"");
            return Ok(1);
        }
    }
    repository.refs().delete(&reference)?;
    if remote {
        println!("Deleted remote-tracking branch {name} (was {}).", super::short_oid(&oid));
    } else {
        // The branch's own config section goes with it, as in Git.
        let mut config = rivet_core::Config::load(&repository.git_dir().join("config"))?;
        if config.remove_section(BStr::new("branch"), Some(BStr::new(name.as_bytes()))) {
            config.write(&repository.git_dir().join("config"))?;
        }
        println!("Deleted branch {name} (was {}).", super::short_oid(&oid));
    }
    Ok(0)
}

fn branch_upstream(repository: &Repository, name: &str) -> Result<Option<RefName>> {
    let config = repository.config();
    let remote = config.get(BStr::new(format!("branch.{name}.remote").as_bytes()))?.map(|v| v.to_owned());
    let merge = config.get(BStr::new(format!("branch.{name}.merge").as_bytes()))?.map(|v| v.to_owned());
    let (Some(remote), Some(merge)) = (remote, merge) else { return Ok(None) };
    if remote == "." {
        return Ok(RefName::from_bytes(&merge).ok());
    }
    let short = merge.strip_prefix(b"refs/heads/").unwrap_or(&merge);
    let mut name = BString::from("refs/remotes/");
    name.extend_from_slice(&remote);
    name.push(b'/');
    name.extend_from_slice(short);
    Ok(RefName::from_bytes(&name).ok())
}

fn rename_branch(
    repository: &mut Repository,
    head: &HeadState,
    old: &RefName,
    new: &RefName,
    force: bool,
    copy: bool,
) -> Result<i32> {
    let Some(oid) = repository.refs().try_read(old)?.and_then(|t| t.oid().ok()) else {
        if head.branch() == Some(old) && !copy {
            // Renaming an unborn current branch just repoints HEAD.
            repository.refs().set_head(new, None)?;
            return Ok(0);
        }
        anyhow::bail!("no branch named '{}'", old.short_name());
    };
    if old == new && !copy {
        return Ok(0);
    }
    if repository.refs().exists(new) {
        if !force {
            anyhow::bail!("a branch named '{}' already exists", new.short_name());
        }
        if head.branch() == Some(new) {
            anyhow::bail!("cannot force update the current branch.");
        }
        repository.refs().delete(new)?;
    }
    let who = repository.committer_signature()?;
    let old_short = old.short_name().to_string();
    let new_short = new.short_name().to_string();
    if copy {
        let reason = format!("Branch: copied {} to {}", old, new);
        repository.set_ref_logged(new, oid, &who, BStr::new(reason.as_bytes()))?;
    } else {
        repository.refs().rename(old, new)?;
        let reason = format!("Branch: renamed {} to {}", old, new);
        if repository.should_log_ref(new) {
            let entry = rivet_core::ReflogEntry::new(oid, oid, who.clone(), BString::from(reason.as_str()));
            repository.refs().append_reflog(new, &entry)?;
        }
        if head.branch() == Some(old) {
            repository.refs().set_head(new, None)?;
        }
    }
    // Carry the branch's configuration across.
    let keys: Vec<(BString, BString)> = repository
        .config()
        .entries()
        .iter()
        .filter(|e| e.section.eq_ignore_ascii_case(b"branch") && e.subsection.as_ref().is_some_and(|s| s.as_slice() == old_short.as_bytes()))
        .map(|e| (e.key.clone(), e.value.clone()))
        .collect();
    if !keys.is_empty() {
        let config = repository.config_mut();
        if !copy {
            config.remove_section(BStr::new("branch"), Some(BStr::new(old_short.as_bytes())));
        }
        for (key, value) in keys {
            let mut flat = BString::from(format!("branch.{new_short}."));
            flat.extend_from_slice(&key);
            config.add(BStr::new(&flat), &value)?;
        }
        repository.write_config()?;
    }
    Ok(0)
}

/// Report a refused checkout the way Git does and return its exit status.
fn report_checkout_refusal(error: rivet_core::Error, op: &str) -> Result<i32> {
    let dirty_block = |paths: &[bstr::BString]| {
        eprintln!("error: Your local changes to the following files would be overwritten by {op}:");
        for path in paths {
            eprintln!("\t{path}");
        }
        eprintln!(
            "{}",
            if op == "checkout" {
                "Please commit your changes or stash them before you switch branches."
            } else {
                "Please commit your changes or stash them before you merge."
            }
        );
    };
    let untracked_block = |paths: &[bstr::BString]| {
        eprintln!("error: The following untracked working tree files would be overwritten by {op}:");
        for path in paths {
            eprintln!("\t{path}");
        }
        eprintln!(
            "{}",
            if op == "checkout" {
                "Please move or remove them before you switch branches."
            } else {
                "Please move or remove them before you merge."
            }
        );
    };
    match error {
        rivet_core::Error::DirtyWorktree { paths } => {
            dirty_block(&paths);
            eprintln!("Aborting");
            Ok(1)
        }
        rivet_core::Error::UntrackedOverwrite(path) => {
            untracked_block(&[path]);
            eprintln!("Aborting");
            Ok(1)
        }
        rivet_core::Error::MergeBlocked { dirty, untracked } => {
            if !dirty.is_empty() {
                dirty_block(&dirty);
            }
            untracked_block(&untracked);
            eprintln!("Aborting");
            Ok(1)
        }
        rivet_core::Error::UnmergedPath { path } => {
            eprintln!("{path}: needs merge");
            eprintln!("error: you need to resolve your current index first");
            Ok(1)
        }
        other => Err(other.into()),
    }
}

const DETACHED_ADVICE: &str = "\nYou are in 'detached HEAD' state. You can look around, make experimental
changes and commit them, and you can discard any commits you make in this
state without impacting any branches by switching back to a branch.

If you want to create a new branch to retain commits you create, you may
do so (now or later) by using -c with the switch command. Example:

  git switch -c <new-branch-name>

Or undo this operation with:

  git switch -

Turn off this advice by setting config variable advice.detachedHead to false
";

fn switch_to(
    repository: &mut Repository,
    destination: Destination,
    label: &str,
    force: bool,
    quiet: bool,
    advise: bool,
) -> Result<i32> {
    let old_head = repository.head()?;
    let from = super::head_reflog_label(repository)?;
    let reason = format!("checkout: moving from {from} to {label}");

    let new_commit = match &destination {
        Destination::Branch(branch) => Some(repository.refs().resolve_oid(branch)?),
        Destination::Detached(oid) => Some(*oid),
    };

    if let Destination::Branch(branch) = &destination
        && old_head.branch() == Some(branch) {
            if force
                && let Some(commit) = new_commit {
                    let tree = repository.commit_tree(commit)?;
                    repository.reset_hard_to_tree(tree)?;
                }
            repository.point_head(HeadTarget::Branch(branch), BStr::new(reason.as_bytes()))?;
            if !quiet {
                show_local_changes(repository)?;
                eprintln!("Already on '{}'", branch.short_name());
            }
            return Ok(0);
        }

    if let Some(commit) = new_commit {
        let tree = repository.commit_tree(commit)?;
        if let Err(error) = repository.checkout_tree(tree, force) {
            return report_checkout_refusal(error, "checkout");
        }
    }

    match &destination {
        Destination::Branch(branch) => {
            repository.point_head(HeadTarget::Branch(branch), BStr::new(reason.as_bytes()))?
        }
        Destination::Detached(oid) => {
            repository.point_head(HeadTarget::Detached(*oid), BStr::new(reason.as_bytes()))?
        }
    }

    if !quiet {
        show_local_changes(repository)?;
        if let HeadState::Detached { oid: old } = &old_head
            && new_commit != Some(*old) {
                eprintln!(
                    "Previous HEAD position was {} {}",
                    super::short_oid(old),
                    super::commit_subject(repository, old)?
                );
            }
        match &destination {
            Destination::Branch(branch) => eprintln!("Switched to branch '{}'", branch.short_name()),
            Destination::Detached(oid) => {
                if advise && !matches!(old_head, HeadState::Detached { .. }) {
                    eprintln!("Note: switching to '{label}'.");
                    eprintln!("{DETACHED_ADVICE}");
                }
                eprintln!(
                    "HEAD is now at {} {}",
                    super::short_oid(oid),
                    super::commit_subject(repository, oid)?
                );
            }
        }
    }
    Ok(0)
}

fn checkout_new_branch(
    repository: &mut Repository,
    name: &str,
    start: &str,
    reset: bool,
    force: bool,
    track: bool,
    quiet: bool,
) -> Result<i32> {
    let branch = valid_branch_name(name)?;
    let exists = repository.refs().exists(&branch);
    if exists && !reset {
        anyhow::bail!("a branch named '{name}' already exists");
    }
    let start_oid = super::resolve_commit(repository, start)
        .map_err(|_| anyhow::anyhow!("'{start}' is not a commit and a branch '{name}' cannot be created from it"))?;
    let old_head = repository.head()?;
    let from = super::head_reflog_label(repository)?;

    // The work tree moves first: a refused checkout must not leave a new branch behind.
    if old_head.oid() != Some(start_oid) || force {
        let tree = repository.commit_tree(start_oid)?;
        if let Err(error) = repository.checkout_tree(tree, force) {
            return report_checkout_refusal(error, "checkout");
        }
    }

    let who = repository.committer_signature()?;
    let reason = if exists {
        format!("branch: Reset to {start}")
    } else {
        format!("branch: Created from {start}")
    };
    repository.set_ref_logged(&branch, start_oid, &who, BStr::new(reason.as_bytes()))?;
    // Creating the branch `HEAD` already names is an update of `HEAD` itself, so its log
    // records the creation too.
    let unborn_same = matches!(&old_head, HeadState::Unborn { branch: b } if *b == branch);
    if unborn_same && repository.should_log_ref(&RefName::head()) {
        let entry = rivet_core::ReflogEntry::new(
            ObjectId::zero(repository.format()),
            start_oid,
            who.clone(),
            BString::from(reason.as_str()),
        );
        repository.refs().append_reflog(&RefName::head(), &entry)?;
    }
    let mut tracking = None;
    if track {
        let upstream = RefName::from_bytes(format!("refs/remotes/{start}").as_bytes())
            .ok()
            .filter(|r| repository.refs().exists(r))
            .or_else(|| RefName::from_bytes(start.as_bytes()).ok().filter(|r| r.is_remote() && repository.refs().exists(r)));
        if let Some(upstream) = upstream {
            tracking = Some(set_upstream(repository, &branch, &upstream)?);
        }
    }
    let reason = format!("checkout: moving from {from} to {name}");
    repository.point_head(HeadTarget::Branch(&branch), BStr::new(reason.as_bytes()))?;
    if let Some(message) = tracking
        && !quiet {
            println!("{message}");
        }
    if !quiet {
        show_local_changes(repository)?;
        if unborn_same {
            eprintln!("Already on '{name}'");
        } else if exists {
            eprintln!("Switched to and reset branch '{name}'");
        } else {
            eprintln!("Switched to a new branch '{name}'");
        }
    }
    Ok(0)
}

fn orphan_branch(repository: &mut Repository, name: &str, quiet: bool) -> Result<i32> {
    let branch = valid_branch_name(name)?;
    if repository.refs().exists(&branch) {
        anyhow::bail!("a branch named '{name}' already exists");
    }
    // `switch --orphan` starts from an empty tree: every tracked file goes.
    let root = repository.worktree()?.require_root("switch")?.to_path_buf();
    let paths: Vec<BString> = repository.index().entries().iter().map(|e| e.path.clone()).collect();
    for path in &paths {
        let path_ref = BStr::new(&path[..]);
        if repository.worktree()?.stat(path_ref)?.is_some() {
            rivet_core::worktree::checkout::remove_path(repository.worktree()?, path_ref)?;
            rivet_core::ops::prune_empty_dirs(&root, path_ref);
        }
    }
    repository.reset_index_to_tree(None)?;
    let from = super::head_reflog_label(repository)?;
    let reason = format!("checkout: moving from {from} to {name}");
    repository.point_head(HeadTarget::Branch(&branch), BStr::new(reason.as_bytes()))?;
    if !quiet {
        eprintln!("Switched to a new branch '{name}'");
    }
    Ok(0)
}

fn checkout_paths(repository: &mut Repository, source: Option<&str>, paths: &[PathBuf], quiet: bool) -> Result<i32> {
    let mut specs = Vec::with_capacity(paths.len());
    for path in paths {
        specs.push(super::worktree_path(repository, path, "checkout")?);
    }
    let (tree, label) = match source {
        Some(spec) => {
            let oid = super::resolve_rev(repository, spec)?;
            let tree = repository.commit_tree(oid)?;
            (Some(tree), super::short_oid(&super::resolve_rev(repository, spec)?))
        }
        None => (None, "the index".to_string()),
    };
    let staged = tree.is_some();
    match repository.restore_paths(tree, &specs, staged, true, true) {
        Ok(count) => {
            if !quiet {
                let noun = if count == 1 { "path" } else { "paths" };
                eprintln!("Updated {count} {noun} from {label}");
            }
            Ok(0)
        }
        Err(rivet_core::Error::PathNotFound(spec)) => {
            eprintln!("error: pathspec '{spec}' did not match any file(s) known to git");
            Ok(1)
        }
        Err(error) => Err(error.into()),
    }
}

/// `MERGE_MSG` without the `#` comment lines Git adds for the editor.
fn strip_comment_lines(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in message.lines_with_terminator() {
        if !line.starts_with(b"#") {
            out.extend_from_slice(line);
        }
    }
    while out.ends_with(b"\n\n") {
        out.pop();
    }
    out
}

/// Git's default merge message for merging `spec` into the current branch.
pub fn merge_message(repository: &Repository, spec: &str) -> String {
    let current = repository
        .head()
        .ok()
        .and_then(|h| h.branch().map(|b| b.short_name().to_string()));
    let what = if repository
        .refs()
        .exists(&RefName::branch(BStr::new(spec.as_bytes())).unwrap_or_else(|_| RefName::head()))
    {
        format!("branch '{spec}'")
    } else if spec.starts_with("refs/remotes/")
        || RefName::from_bytes(format!("refs/remotes/{spec}").as_bytes())
            .is_ok_and(|r| repository.refs().exists(&r))
    {
        format!("remote-tracking branch '{}'", spec.trim_start_matches("refs/remotes/"))
    } else if RefName::tag(BStr::new(spec.as_bytes())).is_ok_and(|r| repository.refs().exists(&r)) {
        format!("tag '{spec}'")
    } else {
        format!("commit '{spec}'")
    };
    // `merge.suppressDest` defaults to the default branch names; elsewhere Git names the target.
    match current {
        Some(branch) if branch != "master" && branch != "main" => format!("Merge {what} into {branch}"),
        _ => format!("Merge {what}"),
    }
}

/// Merge `theirs` into `HEAD`: fast-forward when allowed and possible, otherwise a three-way
/// merge that commits, stops (`no_commit`) or leaves conflicts. Returns Git's exit status.
#[allow(clippy::too_many_arguments)]
pub fn merge_commit(
    repository: &mut Repository,
    theirs: ObjectId,
    label: &str,
    message: &str,
    ff: FastForward,
    no_commit: bool,
    allow_unrelated: bool,
    options: &rivet_core::MergeOptions,
    show_stat: bool,
    action: &str,
    explicit_message: bool,
) -> Result<i32> {
    let git_dir = repository.git_dir().to_path_buf();
    let head = repository.head()?;
    let Some(ours) = head.oid() else {
        // Merging into an unborn branch just points it at the other side.
        let tree = repository.commit_tree(theirs)?;
        if let Err(error) = repository.checkout_tree(tree, false) {
            return report_checkout_refusal(error, "merge");
        }
        let reason = format!("{action} {label}: initial pull");
        repository.move_head(theirs, BStr::new(reason.as_bytes()))?;
        return Ok(0);
    };

    // Git records the pre-merge tip before deciding anything, "Already up to date" included.
    rivet_core::write_orig_head(&git_dir, ours)?;
    if repository.is_ancestor(theirs, ours)? {
        if show_stat {
            println!("Already up to date.");
        }
        return Ok(0);
    }

    if ff != FastForward::Never && repository.is_ancestor(ours, theirs)? {
        if show_stat {
            println!("Updating {}..{}", super::short_oid(&ours), super::short_oid(&theirs));
        }
        let old_tree = repository.commit_tree(ours)?;
        let new_tree = repository.commit_tree(theirs)?;
        if let Err(error) = repository.checkout_tree(new_tree, false) {
            return report_checkout_refusal(error, "merge");
        }
        let note = if explicit_message { "Fast-forward (no commit created; -m option ignored)" } else { "Fast-forward" };
        let reason = if action.starts_with("pull") { format!("{action}: {note}") } else { format!("{action} {label}: {note}") };
        repository.move_head(theirs, BStr::new(reason.as_bytes()))?;
        if show_stat {
            println!("Fast-forward");
        }
        if show_stat {
            super::print_diffstat(repository, Some(old_tree), new_tree)?;
        }
        return Ok(0);
    }
    if ff == FastForward::Only {
        eprintln!("hint: Diverging branches can't be fast-forwarded, you need to either:");
        eprintln!("hint:");
        eprintln!("hint: \tgit merge --no-ff");
        eprintln!("hint:");
        eprintln!("hint: or:");
        eprintln!("hint:");
        eprintln!("hint: \tgit rebase");
        eprintln!("hint:");
        eprintln!("hint: Disable this message with \"git config set advice.diverging false\"");
        eprintln!("fatal: Not possible to fast-forward, aborting.");
        return Ok(128);
    }

    let base = match rivet_core::merge_bases(repository.odb(), ours, theirs) {
        Ok(bases) => bases.first().copied(),
        Err(rivet_core::Error::NoMergeBase { .. }) => None,
        Err(error) => return Err(error.into()),
    };
    if base.is_none() && !allow_unrelated {
        eprintln!("fatal: refusing to merge unrelated histories");
        return Ok(128);
    }

    let mut options = options.clone();
    options.labels = (BString::from("HEAD"), BString::from(label));
    let result = match repository.merge_into_worktree(base, theirs, &options) {
        Ok(result) => result,
        Err(error) => return report_checkout_refusal(error, "merge"),
    };

    for path in &result.paths {
        if result.conflicts.iter().any(|c| c.path == path.path) {
            continue;
        }
        if path.kind.is_some() {
            println!("Auto-merging {}", path.path);
        }
    }
    let mut conflicted = Vec::new();
    for conflict in &result.conflicts {
        use rivet_core::ConflictKind;
        match conflict.kind {
            ConflictKind::Content | ConflictKind::AddAdd | ConflictKind::Mode | ConflictKind::Binary => {
                println!("Auto-merging {}", conflict.path);
                let what = if conflict.kind == ConflictKind::AddAdd { "add/add" } else { "content" };
                println!("CONFLICT ({what}): Merge conflict in {}", conflict.path);
            }
            ConflictKind::ModifyDelete => println!(
                "CONFLICT (modify/delete): {} deleted in {label} and modified in HEAD.  Version HEAD of {} left in tree.",
                conflict.path, conflict.path
            ),
            ConflictKind::DeleteModify => println!(
                "CONFLICT (modify/delete): {} deleted in HEAD and modified in {label}.  Version {label} of {} left in tree.",
                conflict.path, conflict.path
            ),
            ConflictKind::Submodule => println!("CONFLICT (submodule): Merge conflict in {}", conflict.path),
            ConflictKind::RenameDelete | ConflictKind::RenameRename => {
                // Only the entry that carries the message is announced.
                if let Some(message) = &conflict.message {
                    println!("{message}");
                }
            }
        }
        conflicted.push(conflict.path.clone());
    }

    if !conflicted.is_empty() || no_commit {
        let mut merge_msg = BString::from(message);
        merge_msg.push(b'\n');
        if !conflicted.is_empty() {
            merge_msg.extend_from_slice(b"\n# Conflicts:\n");
            for path in &conflicted {
                merge_msg.extend_from_slice(b"#\t");
                merge_msg.extend_from_slice(path);
                merge_msg.push(b'\n');
            }
        }
        let state = rivet_core::MergeState {
            merge_head: vec![theirs],
            merge_msg,
            orig_head: Some(ours),
            head_commit: Some(ours),
            tree: None,
        };
        rivet_core::write_merge_state(&git_dir, &state)?;
        let mode_path = git_dir.join("MERGE_MODE");
        std::fs::write(&mode_path, if ff == FastForward::Never { &b"no-ff"[..] } else { &b""[..] })
            .map_err(|e| anyhow::anyhow!("could not write '{}': {e}", mode_path.display()))?;
        if !conflicted.is_empty() {
            println!("Automatic merge failed; fix conflicts and then commit the result.");
            return Ok(1);
        }
        println!("Automatic merge went well; stopped before committing as requested");
        return Ok(0);
    }

    let tree = repository.write_tree()?;
    let mut body = BString::from(message);
    body.push(b'\n');
    let commit = repository.write_commit_object(tree, &[ours, theirs], BStr::new(&body))?;
    let reason = if action.starts_with("pull") {
        format!("{action}: Merge made by the 'ort' strategy.")
    } else {
        format!("{action} {label}: Merge made by the 'ort' strategy.")
    };
    repository.move_head(commit, BStr::new(reason.as_bytes()))?;
    if show_stat {
        println!("Merge made by the 'ort' strategy.");
    }
    if show_stat {
        let old_tree = repository.commit_tree(ours)?;
        super::print_diffstat(repository, Some(old_tree), tree)?;
    }
    Ok(0)
}

/// Shell-style glob over branch and tag names: `*`, `?` and literal bytes.
pub fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            glob_match(&pattern[1..], text) || (!text.is_empty() && glob_match(pattern, &text[1..]))
        }
        (Some(b'?'), Some(_)) => glob_match(&pattern[1..], &text[1..]),
        (Some(p), Some(t)) if p == t => glob_match(&pattern[1..], &text[1..]),
        _ => false,
    }
}

/// The `M\tpath` lines Git prints after a switch for tracked files with local changes.
fn show_local_changes(repository: &mut Repository) -> Result<()> {
    let mut options = rivet_core::StatusOptions::new();
    options.untracked_files = rivet_core::UntrackedMode::No;
    let status = repository.status(&options)?;
    let mut changes: std::collections::BTreeMap<BString, char> = std::collections::BTreeMap::new();
    for entry in status.staged.iter().chain(status.unstaged.iter()) {
        use rivet_core::ChangeKind;
        let letter = match entry.kind {
            ChangeKind::Added => 'A',
            ChangeKind::Deleted => 'D',
            ChangeKind::TypeChanged => 'T',
            ChangeKind::Unmerged => 'U',
            ChangeKind::Modified => 'M',
        };
        changes
            .entry(entry.path.clone())
            .and_modify(|existing| {
                // Added then modified is still an addition relative to HEAD.
                if *existing != 'A' {
                    *existing = letter;
                }
            })
            .or_insert(letter);
    }
    for (path, letter) in changes {
        println!("{letter}\t{path}");
    }
    Ok(())
}
