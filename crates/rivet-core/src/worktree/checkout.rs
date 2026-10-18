//! Phase 4 — checkout: bringing the work tree to a tree, safely.
//!
//! A checkout is a `diff` followed by a decision followed by writes, and the decision is the
//! entire reason this module is separate from [`crate::worktree::status`]:
//!
//! 1. **Plan.** Walk the current tree and the target tree in parallel and produce one
//!    [`CheckoutEntry`] per path the operation would touch — the write set and the delete set.
//!    Nothing is written.
//! 2. **Validate.** Check every entry in that plan against the work tree and the index, and
//!    refuse the whole operation if any of them would destroy something. Nothing is written.
//! 3. **Apply.** Write the plan. Because step 2 already proved the whole plan safe, a failure
//!    here is an I/O failure rather than a safety failure, and the operation is re-runnable.
//!
//! # The `victim.txt` rule, stated once
//!
//! Plan §5 and the tinyvcs article's bug are the same scenario: a file is modified in the work
//! tree, the target commit does not contain it, and the checkout deletes it — silently
//! destroying work that exists nowhere else. The rule that prevents it, and the rule
//! [`CheckoutPlan::validate`] implements, is:
//!
//! > For every path in the plan, refuse if the work tree file differs from the index **or** the
//! > index differs from `HEAD`.
//!
//! It is stated as two clauses because they catch different things, and the second is the one
//! that is easy to leave out:
//!
//! * **Work tree ≠ index** catches "you edited this file and have not staged it". The obvious
//!   case, and the one most implementations get.
//! * **Index ≠ `HEAD`** catches "you staged a change and the target does not have it" — and,
//!   crucially, "you deleted the file from the index" and "the file was modified in the work
//!   tree and deleted in the target", which is `victim.txt` exactly. A checkout that only
//!   compares the work tree to the index sees a clean file whose index entry matches it, writes
//!   the target's content over it or deletes it, and loses the staged state with no warning.
//!
//! # Caveats that bite
//!
//! * **Untracked files are never overwritten, and there is no `--force` that makes it safe.**
//!   [`crate::error::Error::UntrackedOverwrite`] has no override because an untracked file is in
//!   no object database — losing it loses it permanently. Git's `checkout -f` does clobber
//!   untracked files that are in the way of a *tracked* path, and Rivet refuses instead: this is
//!   a deliberate divergence, recorded here so the differential tests expect it.
//! * **A mode-only change is still a change.** The blob id is identical, so a comparison of ids
//!   alone sees nothing to do; the file's permissions on disk must still be set, and a checkout
//!   that skips it leaves an executable file non-executable and a subsequent `status` reporting
//!   a type change forever.
//! * **Directories are created before the files in them and removed after the files in them.**
//!   A delete pass that ran before the write pass would remove a directory the write pass then
//!   needs, and a write pass that ran in any order other than parents-first creates files in
//!   directories that do not exist.
//! * **Empty directories are left alone.** Git does not track them, so a checkout has no
//!   information about them and must not remove one it did not empty — a user's `build/` with
//!   nothing in it is not the checkout's to delete.
//! * **Case collisions are a real failure on a case-insensitive filesystem.** Two tree entries
//!   whose paths differ only in case cannot both exist on macOS or Windows; the plan must
//!   detect it (using `core.ignorecase`) and refuse, rather than writing one and silently
//!   losing the other.
//! * **The index is not updated here.** A checkout that moves `HEAD` and writes files also
//!   needs the index to match, and the ordering of those three is `repo::Repository`'s
//!   `checkout` / `switch`, because the crash-consistency argument depends on all of them.
//! * **Symlinks are never followed, in either direction.** Writing one creates a link whose
//!   target is the blob's bytes; removing one removes the link. A checkout that resolved a link
//!   before writing would write through it, which is how a repository escapes its own work tree.

use bstr::BString;

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::index::Index;
use crate::object::EntryMode;
use crate::odb::Odb;
use crate::worktree::{StatCache, Worktree};

/// Phase 4 — one filesystem action a checkout will perform.
///
/// # Caveats
///
/// * **`Keep` is not a no-op.** It means the content is already correct and only the file's
///   permissions may need setting — a mode change with an identical blob. Dropping it from the
///   plan makes a `100644` → `100755` checkout do nothing at all.
/// * **`Remove` may be absent from the plan and still happen.** Removing a file can leave an
///   empty directory behind, and whether that directory is removed is a decision made during
///   apply, not a plan entry — because a directory can only be judged empty once every entry
///   under it has been processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CheckoutAction {
    /// The path does not exist in the work tree and will be written.
    Create,
    /// The path exists with different content and will be overwritten.
    Update,
    /// The path is being deleted from the work tree.
    Remove,
    /// The path exists with the right content; only its mode may need setting.
    Keep,
}

/// Phase 4 — one path a checkout will act on.
///
/// # Caveats
///
/// * **`oid` is the *target* blob**, absent for [`CheckoutAction::Remove`]. A caller that wants
///   the file it is replacing has to read it, which is deliberate: keeping a copy of the old
///   content in the plan would make the plan hold the work tree in memory.
/// * **A gitlink (`160000`) has an `oid` that is not in this repository.** The entry plans a
///   `Keep` at best and must never be written or read — Rivet does not implement submodules
///   (plan §1) but must preserve the entry so that a repository containing one is not corrupted
///   by a checkout.
/// * **`mode` is what the file will have afterwards**, and for a symlink on a filesystem with
///   `core.symlinks = false` it is [`EntryMode::Link`] even though a regular file is written
///   holding the target's text. The mode is the repository's intent, not the filesystem's
///   result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutEntry {
    /// The path, work-tree-relative and `/`-separated.
    pub path: BString,
    /// What will be done to it.
    pub action: CheckoutAction,
    /// The blob to write, absent for a removal.
    pub oid: Option<ObjectId>,
    /// The mode the entry will have after the checkout.
    pub mode: Option<EntryMode>,
}

/// Phase 4 — how a checkout should behave.
///
/// # Caveats
///
/// * **`force` skips [`CheckoutPlan::validate`] entirely.** It is what `checkout -f` means and
///   it is the only way to lose work, so it is opt-in per call and never a default. Note that it
///   does *not* enable overwriting untracked files, which Rivet refuses unconditionally.
/// * **`keep_going` turns the first I/O failure into a partial checkout.** Git's `checkout` does
///   not stop at the first write error either, and a caller that wants the two-phase guarantee
///   must leave it off — with it on, the work tree is a mixture of two trees, which is exactly
///   the state the validate-then-write ordering exists to prevent.
/// * **`dry_run` makes apply a no-op and is how `checkout --dry-run` and the safety check in
///   `switch` are expressed.** It does not skip validation, which is the point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutOptions {
    /// Skip the safety check. Loses work by design.
    pub force: bool,
    /// Continue past a failed write rather than stopping.
    pub keep_going: bool,
    /// Plan and validate, but write nothing.
    pub dry_run: bool,
    /// Limit the checkout to a path prefix, empty for the whole tree.
    pub pathspec: BString,
    /// Whether to create directories that the plan implies. Off is for a caller that has already
    /// made them — a `restore` of one file, say.
    pub create_dirs: bool,
}

impl CheckoutOptions {
    /// Phase 4 — the options a plain `rivet checkout <rev>` uses: no force, stop on the first
    /// error, no dry run, the whole tree, directories created.
    pub fn new() -> Self {
        Self {
            force: false,
            keep_going: false,
            dry_run: false,
            pathspec: BString::from(""),
            create_dirs: true,
        }
    }

    /// Phase 4 — set the force flag.
    pub fn force(&mut self, on: bool) -> &mut Self {
        self.force = on;
        self
    }

    /// Phase 4 — turn the operation into a dry run.
    pub fn dry_run(&mut self, on: bool) -> &mut Self {
        self.dry_run = on;
        self
    }

    /// Phase 4 — limit the checkout to a path prefix.
    pub fn pathspec(&mut self, prefix: &bstr::BStr) -> &mut Self {
        self.pathspec = prefix.to_owned();
        self
    }
}

/// Phase 4 — the complete set of actions a checkout will take, computed before anything is
/// written.
///
/// # Caveats
///
/// * **The plan is a value, not a closure over the repository.** It holds object ids and paths
///   only, so a caller can hold it across a `HEAD` move — which is what `switch` does — and
///   validate it against the index as it was before the move.
/// * **`from_tree: None` means an unborn `HEAD`**, where there is nothing to compare against
///   and every entry in the target tree is a creation. It is not the same as an empty tree,
///   which is a real object and compares as "everything in the target is an addition" only by
///   coincidence.
/// * **The plan does not re-check the repository.** Between planning and applying, another
///   process can move refs and rewrite the index; the validation pass is what re-reads the work
///   tree, and it is the caller's job not to apply a plan it has held for a long time.
/// * **Entries are sorted by path**, so that a caller rendering `checkout -n` output gets a
///   stable order and so that the apply pass can create directories in one sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutPlan {
    /// The actions, in path order.
    entries: Vec<CheckoutEntry>,
    /// The tree the work tree is coming from, or `None` for an unborn `HEAD`.
    from_tree: Option<ObjectId>,
    /// The tree the work tree is going to.
    to_tree: ObjectId,
    /// The repository's object format, needed to read the blobs the plan names.
    format: ObjectFormat,
}

impl CheckoutPlan {
    /// Phase 4 — compute the plan for moving the work tree from one tree to another.
    ///
    /// The comparison is a parallel walk of two trees, which is a plain merge because tree
    /// entries are canonically sorted with directories compared as if their names ended in `/`
    /// (see [`crate::object::tree`]) — the sort rule exists precisely so that this walk has no
    /// special cases.
    ///
    /// # Caveats
    ///
    /// * **Recursion happens only where the entries differ.** Two subtrees with the same id are
    ///   the same subtree, and descending into them would walk the whole tree on every checkout
    ///   of a one-file change.
    /// * **A mode change with an equal id is kept in the plan** as [`CheckoutAction::Keep`]; see
    ///   [`CheckoutEntry`].
    /// * **A gitlink is never descended into**, even though the entry's id names a commit and
    ///   the work tree entry looks like a directory. Its content belongs to another repository.
    /// * **`options.pathspec` prunes the walk, and the pruning happens after the entry is
    ///   produced**, so a `Delete` for a path outside the pathspec is not planned — which is
    ///   what makes `checkout <rev> -- <path>` leave the rest of the work tree alone.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] or the tree-parse errors of the object database
    /// if either tree or one of their subtrees does not resolve;
    /// [`crate::error::Error::WrongObjectKind`] if an id that should be a tree is not;
    /// [`crate::error::Error::InvalidPath`] for a pathspec that escapes the work tree.
    pub fn plan(
        format: ObjectFormat,
        from_tree: Option<ObjectId>,
        to_tree: ObjectId,
        odb: &Odb,
        options: &CheckoutOptions,
    ) -> Result<Self> {
        todo!()
    }

    /// Phase 4 — the actions, in path order.
    pub fn entries(&self) -> &[CheckoutEntry] {
        &self.entries
    }

    /// Phase 4 — the tree the work tree is coming from, `None` for an unborn `HEAD`.
    pub fn from_tree(&self) -> Option<ObjectId> {
        self.from_tree
    }

    /// Phase 4 — the tree the work tree is going to.
    pub fn to_tree(&self) -> ObjectId {
        todo!()
    }

    /// Phase 4 — whether the plan would change anything.
    ///
    /// # Caveats
    ///
    /// **`false` does not mean the checkout is unnecessary.** A [`CheckoutAction::Keep`] entry
    /// with a mode difference is a real change and keeps the plan non-empty; an empty plan means
    /// the two trees agree on every path the pathspec covers.
    pub fn is_empty(&self) -> bool {
        todo!()
    }

    /// Phase 4 — the paths whose content the plan would write.
    pub fn written_paths(&self) -> Vec<&bstr::BStr> {
        todo!()
    }

    /// Phase 4 — the paths the plan would delete.
    pub fn removed_paths(&self) -> Vec<&bstr::BStr> {
        todo!()
    }

    /// Phase 4 — how many entries each action accounts for, as `(create, update, remove, keep)`.
    ///
    /// This is the number `checkout -n`'s summary line prints, and the four are separate because
    /// they mean different things to a user: a plan of 10 000 keeps and one create is a
    /// different operation from the reverse.
    pub fn summary(&self) -> (usize, usize, usize, usize) {
        todo!()
    }

    /// Phase 4 — the paths the operation would lose, without raising an error about them.
    ///
    /// The first clause of the `victim.txt` rule, and the list
    /// [`crate::error::Error::DirtyWorktree`] is built from. Exposed separately so that
    /// `checkout -n` and an editor integration can *report* what would be lost rather than
    /// having to catch the error to find out.
    ///
    /// # Caveats
    ///
    /// * **Both clauses are applied**, not just "the work tree file is dirty": a path whose work
    ///   tree file matches the index but whose index entry differs from `HEAD` is included. That
    ///   is the `victim.txt` case — modified and staged, then deleted by the target — and it is
    ///   the one a single-clause implementation misses.
    /// * **An entry that is being created cannot be dirty**, because there is nothing there to
    ///   lose; a `Create` over an existing untracked file is a different failure, caught by
    ///   [`CheckoutPlan::untracked_obstructions`].
    /// * **The whole list is returned**, never the first offender: a user who fixes one path and
    ///   is refused again for the next has been told the wrong thing about the scale of the
    ///   problem.
    /// * **It costs a work tree read per affected path**, because a race between the stat cache
    ///   and the content is exactly what this check exists to catch — an entry whose stat data
    ///   says unchanged is still verified if the plan would remove or overwrite it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::BareRepository`], [`crate::error::Error::IoAtPath`] for a file
    /// that exists and cannot be read, and [`crate::error::Error::InvalidPath`].
    pub fn dirty_paths(
        &self,
        worktree: &Worktree,
        index: &Index,
        stat_cache: &mut StatCache,
    ) -> Result<Vec<BString>> {
        todo!()
    }

    /// Phase 4 — the untracked files the plan would overwrite.
    ///
    /// A file that is in the plan's write set, exists on disk, and is in no index entry is
    /// untracked: it exists nowhere but the work tree, so no checkout can be allowed to touch
    /// it. The list becomes [`crate::error::Error::UntrackedOverwrite`] one path at a time, or
    /// all at once for a caller that wants to report the whole set.
    ///
    /// # Errors
    ///
    /// As [`CheckoutPlan::dirty_paths`].
    pub fn untracked_obstructions(
        &self,
        worktree: &Worktree,
        index: &Index,
    ) -> Result<Vec<BString>> {
        todo!()
    }

    /// Phase 4 — prove the whole plan safe, or refuse it whole.
    ///
    /// This is `victim.txt` as a function. It runs
    /// [`CheckoutPlan::dirty_paths`] and [`CheckoutPlan::untracked_obstructions`] and reports
    /// the first non-empty one as an error, touching nothing.
    ///
    /// # Caveats
    ///
    /// * **It is not incremental and must not be called per path.** The guarantee is about the
    ///   operation as a whole: a path-by-path check leaves the work tree half converted when the
    ///   fifth path is the dirty one.
    /// * **A dirty path that the plan does not touch is not an error.** Editing an unrelated
    ///   file must not block a checkout — that is the difference between this and `git checkout`
    ///   refusing on *any* local modification, which Rivet is deliberately narrower than.
    /// * **`force` in the options bypasses this**, which is why the options are a parameter and
    ///   not a flag on the check: a caller that skips validation should have to say so where it
    ///   calls it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::UntrackedOverwrite`] for the first untracked obstruction — it is
    /// unrecoverable, so it outranks the dirty case — and
    /// [`crate::error::Error::DirtyWorktree`] with every dirty path.
    pub fn validate(
        &self,
        worktree: &Worktree,
        index: &Index,
        stat_cache: &mut StatCache,
        options: &CheckoutOptions,
    ) -> Result<()> {
        todo!()
    }

    /// Phase 4 — write the plan to the work tree.
    ///
    /// # Caveats
    ///
    /// * **This does not validate.** A caller that has not run [`CheckoutPlan::validate`] is
    ///   choosing to lose work; the two are separate functions rather than one atomic
    ///   `checkout` precisely so that the ordering is visible at the call site — and because
    ///   `repo::Repository::checkout` moves `HEAD` between the two.
    /// * **Creates run before updates, and removals last.** A rename-like pair (a path that is
    ///   both created and another removed) must not delete a file the create needs, and the
    ///   ordering is what makes a create of `a/b/c` work when `a/b` does not exist yet.
    /// * **Directories are created lazily, one level at a time**, rather than by `create_dir_all`
    ///   on the whole path — because a `create_dir_all` follows an existing symlink in the
    ///   middle of a path, which is a way to write outside the work tree.
    /// * **The file is written to a temporary name and renamed**, but only for a create: an
    ///   update that renamed would break a hard link the user has, and Git itself writes in
    ///   place. The temporary-and-rename form is reserved for the case where a torn file would
    ///   be a new file rather than a modified one.
    /// * **A `Keep` with a mode difference still chmods.** See [`CheckoutEntry`].
    /// * **Removals do not prune directories except ones the plan emptied**, and never a
    ///   directory the user had before the checkout — an empty directory that was already there
    ///   is not the checkout's to remove.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] for a failed create, write, chmod or unlink, and
    /// [`crate::error::Error::ObjectNotFound`] for a blob the plan names that the database does
    /// not have — which can only happen if the object was removed between planning and applying.
    pub fn apply(&self, worktree: &Worktree, odb: &Odb) -> Result<()> {
        todo!()
    }

    /// Phase 4 — the index entries this plan should produce.
    ///
    /// The other half of a checkout: writing files is not enough, because `HEAD` moves and the
    /// index must describe the new `HEAD` or the next `status` reports every path as
    /// staged-deleted and unstaged-added at once.
    ///
    /// # Caveats
    ///
    /// * **The stat data is not filled in here.** Building entries needs the filesystem, which
    ///   means it needs the checkout to have already happened — so a caller builds the entries
    ///   after [`CheckoutPlan::apply`] with [`crate::worktree::build_index_entry`].
    /// * **Paths outside the plan's pathspec are not in the result**, and the caller must merge
    ///   rather than replace: a `restore <path>` that replaced the index wholesale would drop
    ///   every other entry.
    /// * **A `Remove` produces a tombstone, not an entry.** The caller drops the path from the
    ///   index; this returns the ids and modes of the paths that remain, and a caller looking
    ///   for the removals uses [`CheckoutPlan::removed_paths`].
    /// * **The object format comes from the plan**, not from an argument: the plan was built
    ///   against one repository's format and a caller-supplied second source would let the two
    ///   disagree, producing entries whose ids are the wrong width.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if the plan names an object the database lacks.
    pub fn index_entries(&self, odb: &Odb) -> Result<Vec<crate::index::IndexEntry>> {
        todo!()
    }
}

/// Phase 4 — read a blob for checkout, applying the filesystem's symlink setting.
///
/// The one place a blob's bytes become a filesystem action, and therefore the one place the
/// `core.symlinks` decision is made: with symlinks enabled the target bytes become a symlink,
/// and with them disabled they become a regular file's contents — which is what a repository
/// checked out on Windows and copied to Unix looks like, and what Rivet must be able to
/// reproduce.
///
/// # Caveats
///
/// * **A gitlink is never written.** A `160000` entry names a commit in another repository and
///   has no blob to read; a caller that reached here with one has a bug, and refusing beats
///   writing an empty directory where a submodule should be.
/// * **The mode decides, not the content.** A blob that happens to contain a plausible path is
///   a file, and treating it as a link because its bytes look like a target is how a checkout
///   creates a symlink out of an ordinary file — which is a path traversal if the bytes say
///   `/etc/passwd`.
///
/// # Errors
///
/// [`crate::error::Error::ObjectNotFound`] for a missing blob;
/// [`crate::error::Error::WrongObjectKind`] if the id is not a blob;
/// [`crate::error::Error::IoAtPath`] if the file cannot be created, written or renamed.
pub fn write_blob(
    worktree: &Worktree,
    path: &bstr::BStr,
    oid: ObjectId,
    mode: EntryMode,
    odb: &Odb,
) -> Result<()> {
    todo!()
}

/// Phase 4 — delete a path from the work tree.
///
/// # Caveats
///
/// * **A missing file is not an error.** A delete that already happened — because the user
///   removed the file, or because a previous interrupted checkout got this far — is the
///   intended end state, and reporting it would make the operation un-resumable.
/// * **A symlink is removed, not followed**, so a link pointing outside the work tree does not
///   delete its target.
/// * **A directory is not removed.** Only files and links are; the empty-directory pruning is
///   a separate pass, because whether a directory is now empty depends on every removal having
///   happened.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if the unlink fails for a reason other than absence, and
/// [`crate::error::Error::InvalidPath`] for a path that escapes the work tree.
pub fn remove_path(worktree: &Worktree, path: &bstr::BStr) -> Result<()> {
    todo!()
}

/// Phase 4 — whether two paths in one tree would collide on a case-insensitive filesystem.
///
/// # Caveats
///
/// * **This is a property of the filesystem, not of the tree.** `README` and `readme` are two
///   legal entries in a Git tree and the comparison only matters when `core.ignorecase` is set;
///   a caller that always checked would refuse to check out a perfectly valid repository on
///   Linux.
/// * **The comparison is ASCII-only and is not Unicode-aware**, matching Git: a filesystem that
///   folds `ß` to `ss` is not something Git models, and pretending to would make Rivet refuse
///   trees Git checks out.
/// * **Path components are compared, not whole paths.** `a/b` and `A/b` collide; so do `a/b`
///   and `a/B`, but the second pair differs at a deeper level and the message a user needs
///   names the two components.
pub fn paths_collide(a: &bstr::BStr, b: &bstr::BStr, ignore_case: bool) -> bool {
    todo!()
}
