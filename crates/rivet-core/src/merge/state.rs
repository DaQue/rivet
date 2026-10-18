//! Phase 9 — the merge state: the files that make a merge resumable, and `--abort`.
//!
//! A git directory mid-merge holds four pieces of state, and each exists so that a *different*
//! later command can do its job without being told what happened:
//!
//! * **`MERGE_HEAD`** — the commit(s) being merged in. One line each, in object-id hex. Its
//!   presence is the definition of "a merge is in progress", and `commit` reads it to decide
//!   whether to write a merge commit and with which parents.
//! * **`MERGE_MSG`** — the commit message the merge *would* have used, pre-filled with a
//!   `CONFLICT (content): ...` and `# Conflicts:` section. `commit` uses it as the default
//!   message, and the user edits it. Writing it is why a conflicted merge followed by `commit -m`
//!   still produces a sensible message rather than an empty one.
//! * **`ORIG_HEAD`** — where `HEAD` was before the merge started. `merge --abort` resets to it,
//!   and so does a failed `pull`; it is written *before* anything else changes, because the point
//!   of it is to survive a merge that dies mid-way.
//! * **The index's conflict stages** — the three versions per conflicted path, which are the only
//!   copy of the base content once the merge has overwritten the work tree. Losing them makes
//!   `git mergetool` and `checkout --merge` impossible. They live in the index and are written by
//!   [`crate::merge::write_conflict_stages`], not here.
//!
//! # Caveats that bite
//!
//! * **`MERGE_HEAD` is a list and can hold more than one id.** An octopus merge (`git merge a b
//!   c`) has one line per commit, and a reader that parsed only the first line silently drops the
//!   rest — the commit then has the wrong number of parents and the merge is not recorded.
//! * **An empty `MERGE_HEAD` is not "no merge".** A zero-byte file is a corrupt state, not an
//!   absent one; the check for "is a merge in progress" is *existence* of the file, and a caller
//!   that read the contents and tested for emptiness would report a merge that `--abort` then
//!   cannot find.
//! * **`ORIG_HEAD` is not removed by a successful merge**, deliberately: Git keeps it until the
//!   next operation that overwrites it, which is what makes `reset ORIG_HEAD` work after a merge
//!   that succeeded. `clear_merge_state` is about `MERGE_HEAD` and `MERGE_MSG` only.
//! * **`--abort` refuses rather than guesses.** With no merge in progress it is
//!   [`crate::error::Error::NoMergeInProgress`]; with a work tree whose changes are unrelated to
//!   the merge it is [`crate::error::Error::DirtyWorktree`], because resetting to `ORIG_HEAD`
//!   would discard them. Git refuses in both cases and Rivet must too — an `--abort` that
//!   discarded the user's edits would be the worst possible bug in a merge.
//! * **`--abort` restores the work tree as well as the index**, which is the part a first
//!   implementation forgets: resetting the index leaves the conflicted files — markers and all —
//!   sitting in the work tree, and `status` then reports a clean index with a dirty work tree,
//!   which is a state the user cannot get out of without `checkout -- .`.
//! * **A merge that fast-forwarded leaves nothing to abort.** There is no merge commit and no
//!   `MERGE_HEAD`; `--abort` reports [`crate::error::Error::NoMergeInProgress`] and the way back is
//!   `reset --hard ORIG_HEAD`, which the caller should tell the user. Writing a `MERGE_HEAD` for a
//!   fast-forward so that `--abort` "works" would make `commit` write a merge commit that Git does
//!   not write.

use std::path::Path;

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::index::Index;
use crate::odb::Odb;
use crate::worktree::Worktree;

/// Phase 9 — the file naming the commit(s) being merged in.
///
/// # Caveats
///
/// The name is relative to the **git directory**, not the work tree, so a caller must join it
/// with the git directory it discovered — an implementation that joined it with the work tree
/// writes to `<worktree>/MERGE_HEAD`, where nothing will ever read it and `status` will report no
/// merge. The same applies to every other file named in this module.
pub const MERGE_HEAD: &str = "MERGE_HEAD";

/// Phase 9 — the file holding the merge's proposed commit message.
///
/// # Caveats
///
/// Written even when the merge is clean, because `commit` after a clean `merge --no-commit` uses
/// it. A caller that only wrote it on conflict makes `merge --no-commit` followed by `commit`
/// produce an empty message.
pub const MERGE_MSG: &str = "MERGE_MSG";

/// Phase 9 — the file recording where `HEAD` was before the merge.
///
/// # Caveats
///
/// Distinct from `refs/heads/*/ORIG_HEAD`-style per-ref reflog entries and from the reflog
/// itself: this is a single file holding a single id, it is not append-only, and it is not
/// per-branch. It is overwritten by every operation that uses it.
pub const ORIG_HEAD: &str = "ORIG_HEAD";

/// Phase 9 — the state of an in-progress merge.
///
/// # Caveats
///
/// * **`merge_head` is a `Vec` and is never empty in a valid state**, per the module caveat about
///   octopus merges and about an empty file being corruption rather than absence. A caller
///   reading [`read_merge_state`] gets `None` for "no merge" and `Some` with a non-empty list for
///   a merge; an empty list inside a `Some` means the file was corrupt and the implementation
///   must say so rather than return a state with no parents.
/// * **`merge_msg` has already been through the "clean up the subject line" transformation** when
///   it was written, so a caller must not normalise it again — collapsing whitespace twice is
///   idempotent, but re-wrapping is not, and the message is what ends up in the commit.
/// * **`head_commit` is `None` for a merge into an unborn branch.** `git merge` onto a branch with
///   no commits fast-forwards and writes no state at all, but a merge *into* an unborn `HEAD` with
///   an explicit `--no-ff` is representable, and the field exists so a caller does not have to
///   special-case it by reading `HEAD` twice.
/// * **`tree` is the merged tree when the merge was clean and `None` when it was not.** It is
///   stored so that a `merge --no-commit` followed later by `commit` does not have to re-run the
///   merge; a caller that recomputed it would get a different answer if the work tree changed in
///   between, and the answer it should commit is the one the merge produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeState {
    /// The commits being merged in, in the order they were given.
    pub merge_head: Vec<ObjectId>,
    /// The proposed commit message.
    pub merge_msg: BString,
    /// Where `HEAD` was before the merge, from `ORIG_HEAD`.
    pub orig_head: Option<ObjectId>,
    /// The commit `HEAD` pointed at when the merge started, or `None` if unborn.
    pub head_commit: Option<ObjectId>,
    /// The merged tree, when the merge was clean.
    pub tree: Option<ObjectId>,
}

impl MergeState {
    /// Phase 9 — an empty state, for a merge about to record itself.
    pub fn new() -> Self {
        Self {
            merge_head: Vec::new(),
            merge_msg: BString::default(),
            orig_head: None,
            head_commit: None,
            tree: None,
        }
    }

    /// Phase 9 — whether more than one commit is being merged in.
    ///
    /// # Caveats
    ///
    /// An octopus merge, which `commit` must turn into a commit with that many parents. It is not
    /// an error and it is not rare enough to ignore: `git merge topic1 topic2` is a normal thing
    /// to type, and the resulting commit's parent list is what makes the history reachable.
    pub fn is_octopus(&self) -> bool {
        self.merge_head.len() > 1
    }

    /// Phase 9 — the first commit being merged in.
    ///
    /// # Caveats
    ///
    /// `None` only for a state that was constructed empty; a state read from disk always has at
    /// least one. The "first" is the one Git's messages refer to when they name a single other
    /// commit, which is why the order of `merge_head` is preserved from the command line.
    pub fn first_parent(&self) -> Option<ObjectId> {
        self.merge_head.first().copied()
    }

    /// Phase 9 — the message, as bytes.
    ///
    /// # Caveats
    ///
    /// Returns bytes rather than a `str`: the message is user data from a file on disk and is not
    /// guaranteed to be UTF-8. A caller that displayed it by lossy conversion is fine; one that
    /// wrote it into a commit by lossy conversion would change the commit's id.
    pub fn message(&self) -> &BStr {
        todo!()
    }
}

impl Default for MergeState {
    /// Phase 9 — the defaults of [`MergeState::new`].
    fn default() -> Self {
        todo!()
    }
}

/// Phase 9 — write the merge state into the git directory.
///
/// # Caveats
///
/// * **`ORIG_HEAD` is written before `MERGE_HEAD`**, because the point of `ORIG_HEAD` is to
///   survive a failure partway through and a caller that wrote them in the other order leaves a
///   window in which a merge is "in progress" with no way back. The ordering is a correctness
///   requirement, not a style choice.
/// * **The files are written with the same lock-then-rename discipline as a ref.** A partially
///   written `MERGE_HEAD` — a truncated id — is a state that no command can interpret, and a
///   reader is likely to hit it, because the file's presence is what tells other processes a
///   merge is running.
/// * **`MERGE_MSG` is written with a trailing newline and comments already in place**, so the
///   file is what `commit` will present to the user. The `# Conflicts:` section is part of the
///   message text and is stripped by `commit`'s comment handling, not by this function.
/// * **An existing `MERGE_HEAD` is overwritten, not refused.** Refusing is
///   [`crate::error::Error::MergeInProgress`] and it belongs to the caller that starts a merge,
///   *before* it touches anything — a state-writer that refused would fail after the work tree
///   had already been modified.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] for a file that cannot be written, and
/// [`crate::error::Error::LockHeld`] if another process holds the lock.
pub fn write_merge_state(git_dir: &Path, state: &MergeState) -> Result<()> {
    todo!()
}

/// Phase 9 — read the merge state, if a merge is in progress.
///
/// # Caveats
///
/// * **`Ok(None)` means no merge**, decided by the *existence* of `MERGE_HEAD`; see the module
///   caveat about an empty file. A file that exists and does not parse is an error, not `None`.
/// * **`ORIG_HEAD` and `MERGE_MSG` are optional and their absence is not an error.** A merge
///   started by a tool that wrote only `MERGE_HEAD` is readable, and `orig_head` is then `None` —
///   which makes `--abort` unable to restore anything and must be reported by that caller rather
///   than papered over here.
/// * **The ids are parsed against the repository's format**, which is a parameter and not a
///   guess: a SHA-256 repository's `MERGE_HEAD` is 64 hex characters, and a parser fixed at 40
///   reads it as a truncated id or rejects it.
///
/// # Errors
///
/// [`crate::error::Error::CorruptObject`]-style malformed-id reporting for a `MERGE_HEAD` line
/// that is not a valid id: [`crate::error::Error::InvalidHex`]. And
/// [`crate::error::Error::IoAtPath`] for a file that cannot be read.
pub fn read_merge_state(
    git_dir: &Path,
    format: crate::hash::ObjectFormat,
) -> Result<Option<MergeState>> {
    todo!()
}

/// Phase 9 — remove `MERGE_HEAD` and `MERGE_MSG`.
///
/// # Caveats
///
/// * **`ORIG_HEAD` is left alone**, per the module caveat: it outlives the merge on purpose.
/// * **The index's conflict stages are not touched.** Clearing the state is what a *successful*
///   commit does after it has written the merge commit, at which point the index has already been
///   resolved; a caller that cleared the state to "cancel" a conflicted merge leaves the stages
///   behind and must run [`abort_merge`] instead.
/// * **Removing a file that is not there is not an error.** Idempotent, because both `commit` and
///   `--abort` call it on paths where the other may already have.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] for a file that exists and cannot be removed.
pub fn clear_merge_state(git_dir: &Path) -> Result<()> {
    todo!()
}

/// Phase 9 — whether a merge is in progress.
///
/// # Caveats
///
/// Existence of `MERGE_HEAD`, not a parse — so it is cheap enough for `status` to call on every
/// invocation, and it must not be replaced by a parse for that reason. A caller that needs the
/// state calls [`read_merge_state`] and pays for it once.
pub fn merge_in_progress(git_dir: &Path) -> bool {
    git_dir.join("MERGE_HEAD").exists()
}

/// Phase 9 — record where `HEAD` was, for a later `reset` or `--abort`.
///
/// # Caveats
///
/// Called by every operation that moves `HEAD` destructively — merge, rebase, reset, pull — with
/// the value from *before* the move. Writing it after the move is a bug that turns `ORIG_HEAD`
/// into a pointer at the new state, which makes `reset ORIG_HEAD` a no-op.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if the file cannot be written.
pub fn write_orig_head(git_dir: &Path, oid: ObjectId) -> Result<()> {
    todo!()
}

/// Phase 9 — undo a merge: restore the index and the work tree to before it started.
///
/// # Caveats
///
/// * **The safety checks come first and they are not optional.** Before touching anything, this
///   refuses when there is no merge in progress
///   ([`crate::error::Error::NoMergeInProgress`]) and when the work tree holds changes that are
///   unrelated to the merge ([`crate::error::Error::DirtyWorktree`], carrying every offending
///   path). The check is the same one [`crate::worktree::CheckoutPlan::validate`] makes for a
///   checkout to `ORIG_HEAD`'s tree, and it must be that function rather than a reimplementation —
///   the whole of plan §5's `victim.txt` rule is that this decision happens in one place.
/// * **`ORIG_HEAD` is the destination, not `HEAD`.** If `ORIG_HEAD` is missing the abort cannot
///   proceed and must say so rather than resetting to something it guessed.
/// * **The index is restored from the target tree, not from the merge base.** `--abort` means
///   "pretend the merge never happened", which is `reset --merge ORIG_HEAD`, and an implementation
///   that restored the base instead would leave the branch looking like it had been merged
///   partially.
/// * **`MERGE_HEAD` and `MERGE_MSG` are removed last.** If the restore fails partway, the merge
///   state is still on disk and `--abort` can be run again; clearing it first leaves a
///   half-restored work tree with no record of what was happening.
/// * **`--abort` on a fast-forwarded merge is [`crate::error::Error::NoMergeInProgress`]**; see
///   the module caveat. The message the caller prints should mention `reset --hard ORIG_HEAD`.
///
/// # Errors
///
/// [`crate::error::Error::NoMergeInProgress`], [`crate::error::Error::DirtyWorktree`],
/// [`crate::error::Error::UntrackedOverwrite`], [`crate::error::Error::BareRepository`] when there
/// is no work tree to restore, and the I/O errors of the checkout it performs.
pub fn abort_merge(
    worktree: &Worktree,
    index: &mut Index,
    odb: &Odb,
    git_dir: &Path,
) -> Result<()> {
    todo!()
}
