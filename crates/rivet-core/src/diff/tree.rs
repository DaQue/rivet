//! Phase 8 — tree, index and work tree diffs: pairing entries and calling the line diff in.
//!
//! The middle layer of [`crate::diff`]. Three entry points live here, and they differ only in
//! where their two sides come from:
//!
//! * [`tree_diff`] — two tree objects, recursed in parallel. `git diff <commit> <commit>`.
//! * [`index_diff`] — the index's stage-0 entries against one tree. `git diff --staged`.
//! * [`work_tree_diff`] — the index against the filesystem. `git diff` with no arguments.
//! * [`blob_diff`] — two blobs, no trees and no index, for `rivet diff <blob> <blob>`.
//!
//! All three produce the same [`TreeDiff`], because everything above them — the unified
//! renderer, `--stat`, `--name-status` — should not care which two things were compared.
//!
//! # The walk is a merge, not a recursion
//!
//! Trees are sorted by Git's rule (directories compared as if their names ended in `/`), and
//! that rule exists precisely so that diffing two trees is a single linear merge of two sorted
//! lists. A recursive "for each entry in A, look up the entry in B" is the same answer and a
//! different complexity class, and it also loses the ability to report order — which the
//! `--name-status` output's stability depends on.
//!
//! # Caveats that bite
//!
//! * **A tree entry named `.git` never appears and must never be followed.** The parser rejects
//!   it, so a tree that reached here cannot contain one; a work tree walk can *see* a `.git`
//!   file (a gitfile) and must skip it rather than treat the repository's own metadata as
//!   content. See [`crate::worktree`].
//! * **A gitlink is a leaf, not a directory.** Descending into a `160000` entry would diff
//!   another repository's files against this one's; the entry is compared as a single value and
//!   the change is `Subproject commit <old> -> <new>`.
//! * **A mode change with equal content is still a change.** The ids match and there are no
//!   hunks, but the file has to appear in the result or [`crate::diff::format_unified`] would
//!   never be given the chance to print `old mode`/`new mode`.
//! * **A tree entry and a file of the same name is a type change, not a delete plus an add.**
//!   `TreeDiff` reports it once, with one side's mode being `40000` — and the line diff is then
//!   skipped, because one side is not a file at all.
//! * **Nothing here decides what to do with an unreadable object.** A tree entry whose object is
//!   missing from the database is an error, not a "the file is empty now"; see the `# Errors`
//!   sections.

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::index::{Index, IndexEntry};
use crate::object::EntryMode;
use crate::odb::Odb;
use crate::worktree::{StatCache, Worktree};

use crate::diff::unified::{DiffStat, Hunk};

/// Phase 8 — one changed path, with its hunks.
///
/// The unit of a diff: a path, its two sides' identities, and the hunks that take one to the
/// other. Every output format — unified, `--stat`, `--name-status`, `--numstat` — is a
/// projection of a `Vec<FileDiff>`, which is why this type is the one the CLI renders and not
/// the text.
///
/// # Caveats
///
/// * **A `FileDiff` can exist with no hunks.** Three cases, all real: a mode-only change, a
///   binary file (content differs, hunks are suppressed), and a gitlink whose commit id moved.
///   A renderer that treated "no hunks" as "not a change" would drop all three.
/// * **`old_path` and `new_path` are equal for everything Rivet currently produces**, because
///   rename detection is not implemented (plan §4 Phase 8). They are two fields rather than one
///   so that adding rename detection later is not a change to every caller — a rename will then
///   be one `FileDiff` with differing paths rather than a delete and an add.
/// * **`old_mode`/`new_mode` are [`EntryMode`], not `u32`.** A `None` on one side means the file
///   is new or deleted, which is why the modes are optional while the ids' presence is implied
///   by the same `Option` on the path's existence — keeping them separate is what lets a
///   mode-only change be spelled out.
/// * **`is_binary` suppresses the hunks, it does not delete them.** An implementation may leave
///   `hunks` empty for a binary file; a caller must branch on `is_binary` before rendering and
///   never on `hunks.is_empty()`.
/// * **`algorithm` records which line diff produced the hunks**, because the hunks are not
///   reproducible without it — see [`crate::diff::DiffAlgorithm`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// The path on the old side, or `None` when the file is new.
    pub old_path: Option<BString>,
    /// The path on the new side, or `None` when the file is deleted.
    pub new_path: Option<BString>,
    /// The old side's mode, or `None` when it did not exist.
    pub old_mode: Option<EntryMode>,
    /// The new side's mode, or `None` when it does not exist.
    pub new_mode: Option<EntryMode>,
    /// The old side's object id, or `None` when it did not exist.
    pub old_oid: Option<ObjectId>,
    /// The new side's object id, or `None` when it does not exist.
    pub new_oid: Option<ObjectId>,
    /// The hunks, in file order. Empty for a binary file, a mode-only change or a gitlink.
    pub hunks: Vec<Hunk>,
    /// Whether either side was detected as binary.
    pub is_binary: bool,
    /// Whether the file is an addition.
    pub is_new: bool,
    /// Whether the file is a deletion.
    pub is_deleted: bool,
    /// Which algorithm produced `hunks`.
    pub algorithm: crate::diff::DiffAlgorithm,
}

impl FileDiff {
    /// Phase 8 — the path to report the diff under.
    ///
    /// # Caveats
    ///
    /// * **A deletion reports its old path**, and an addition its new one; the fallback exists so
    ///   a caller has one name to print without branching. For everything Rivet produces today
    ///   the two are equal, so the only case where the choice is observable is one a future
    ///   rename detector creates.
    /// * **A `FileDiff` with neither path is a caller bug**, not a representable state — it is
    ///   spelled out here rather than enforced by the type because the alternative is a
    ///   three-variant enum for a case that cannot arise from a tree walk. The implementation
    ///   must not panic on it; see the crate's "never panic on repository data" rule.
    pub fn path(&self) -> &BStr {
        todo!()
    }

    /// Phase 8 — whether the change is only a mode change.
    ///
    /// # Caveats
    ///
    /// The precise condition: the ids are equal and present, the modes are present and differ. A
    /// gitlink whose target moved is *not* this — its ids differ — even though it also has no
    /// hunks, and conflating the two would make `--stat` print `0 insertions, 0 deletions` for a
    /// submodule bump instead of the one-line change it is.
    pub fn is_mode_only(&self) -> bool {
        todo!()
    }

    /// Phase 8 — whether the file was replaced by something that is not a regular file, or the
    /// reverse.
    ///
    /// # Caveats
    ///
    /// Type change here means the *kind* changed — a blob became a symlink, a directory became a
    /// file — which is what `git diff` prints as `T` in `--name-status`. It is not a mode change
    /// between `100644` and `100755`, which is [`FileDiff::is_mode_only`].
    pub fn is_type_change(&self) -> bool {
        self.old_mode.is_some() && self.new_mode.is_some() && self.old_mode.unwrap() != self.new_mode.unwrap()
    }

    /// Phase 8 — the counts `--stat` and `--numstat` need.
    ///
    /// # Caveats
    ///
    /// * **A binary file has no counts**, and the answer is a flag rather than two zeros, because
    ///   `--numstat` prints `-\t-\t<path>` for one and `0\t0\t<path>` means something different
    ///   (a mode-only or gitlink change).
    /// * **The counts come from the hunks' line kinds, not from the buffers.** Counting lines in
    ///   the two files and subtracting is wrong whenever a line is both added and removed before
    ///   the hunk builder sees it, which happens on every reordering.
    pub fn stat(&self) -> DiffStat {
        todo!()
    }
}

/// Phase 8 — every changed path between two sides, plus the totals.
///
/// # Caveats
///
/// * **`files` is sorted by path**, in Git's tree order for a tree diff and in index order for
///   an index diff. Both are byte order over the path, so the two agree, and the output is
///   stable across runs — which the differential tests compare against.
/// * **`stats` is not `files.iter().map(stat)` lazily.** It is the same information; it is stored
///   because `--stat` needs the totals and recomputing them per render is the kind of thing a
///   caller forgets. Keep it consistent with `files` on every construction.
/// * **An empty `TreeDiff` is the common case and is not an error.** "Nothing changed" is what
///   makes `git diff` exit 0 silently, and a caller must not treat the empty result as a failure.
/// * **A pathspec-filtered `TreeDiff` is still a `TreeDiff`.** Nothing marks it as filtered, so a
///   caller that wants to say "no changes in this path" rather than "no changes" has to remember
///   the pathspec it passed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeDiff {
    /// The changed files, sorted by path.
    pub files: Vec<FileDiff>,
    /// The per-file totals, in the same order as `files`.
    pub stats: Vec<DiffStat>,
}

impl TreeDiff {
    /// Phase 8 — an empty diff.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            stats: Vec::new(),
        }
    }

    /// Phase 8 — whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Phase 8 — how many files changed.
    pub fn len(&self) -> usize {
        todo!()
    }

    /// Phase 8 — the diff for one path, if it changed.
    ///
    /// # Caveats
    ///
    /// The lookup is linear over `files` and is meant for a caller checking one or two paths
    /// (a post-checkout hook, a test assertion), not for a caller iterating; the latter already
    /// has the slice.
    pub fn path(&self, path: &BStr) -> Option<&FileDiff> {
        todo!()
    }

    /// Phase 8 — the total insertions and deletions across every file.
    ///
    /// # Caveats
    ///
    /// Binary files contribute nothing and are not counted as zero-line changes; the caller that
    /// needs to know how many were skipped reads [`FileDiff::is_binary`] from `files`.
    pub fn totals(&self) -> (usize, usize) {
        todo!()
    }

    /// Phase 8 — keep only the files under a path prefix.
    ///
    /// # Caveats
    ///
    /// * **The prefix matches path components, not bytes**, the same rule as
    ///   [`crate::index::IndexExt::under`]: `src` must not match `srcfile.c`.
    /// * **An empty prefix keeps everything**, which makes it a legal no-op rather than a filter
    ///   that matches nothing.
    /// * **This mutates `files` and `stats` together.** Dropping from one and not the other is
    ///   how `--stat` ends up disagreeing with the patch it is printed above; if the two are ever
    ///   allowed to diverge, they should be one field, not two.
    pub fn filter_pathspec(&mut self, prefix: &BStr) {
        todo!()
    }
}

/// Phase 8 — the diff of one blob against another.
///
/// The blob-level entry point, and the one a caller with two object ids and no trees uses —
/// `rivet diff <blob> <blob>`, an editor integration, a `check-attr` precursor.
///
/// # Caveats
///
/// * **`old: None` means the file is new** and `new: None` means it is deleted; both `None` is
///   not a diff of anything and returns a [`FileDiff`] with no hunks rather than an error,
///   because a tree walk pairing entries can legitimately produce it for a path that vanished
///   between two reads.
/// * **The mode is a parameter, not read from the blob**, because a blob has no mode — the mode
///   lives in the tree entry or the index entry that named it, and a mode change with equal
///   content is a diff with no hunks.
/// * **The content is read in full.** Binary detection needs only the first 8000 bytes of each
///   side, but the line diff needs all of it, and there is no partial form of a hunk.
/// * **A gitlink's ids are never looked up.** A `160000` entry's "old" and "new" are commits in
///   another repository, so [`crate::error::Error::ObjectNotFound`] here would be a bug rather
///   than a corruption report; a gitlink pair is reported as a one-line change with no hunks.
/// * **`path` is the name for both sides.** Rename detection is not implemented, so a caller
///   that needs two paths sets them on the returned [`FileDiff`] itself.
///
/// # Errors
///
/// [`crate::error::Error::ObjectNotFound`] for a blob the database lacks;
/// [`crate::error::Error::WrongObjectKind`] if an id names a tree, a commit or a tag;
/// [`crate::error::Error::NotImplemented`] for [`crate::diff::DiffAlgorithm::Histogram`].
pub fn blob_diff(
    format: ObjectFormat,
    path: &BStr,
    old: Option<(ObjectId, EntryMode)>,
    new: Option<(ObjectId, EntryMode)>,
    odb: &Odb,
    options: &crate::diff::DiffOptions,
) -> Result<FileDiff> {
    todo!()
}

/// Phase 8 — the diff between two trees.
///
/// # Caveats
///
/// * **`old` or `new` may be the empty tree's id**, which is how `git diff` against a root commit
///   is expressed: pass the empty tree rather than `None`, and every path is an addition. The
///   empty tree is a real object; a caller that passed `None` would get a different (and
///   differently-spelled) result, so the two are not interchangeable.
/// * **Subtrees are recursed in parallel, not diffed whole.** A changed file three directories
///   down must be reported as `a/b/c.txt`, not as the subtree `a/b` having a different id — the
///   latter is a valid and useless answer, and it is what a naive `Tree::get`-based implementation
///   produces.
/// * **An unchanged subtree short-circuits on its id.** Two entries with the same id are equal
///   without reading either, which is the whole performance story of a tree diff: the walk
///   descends only into trees that actually differ.
/// * **Options are applied per file, not per tree.** `pathspec` filters the result rather than
///   the walk (no directory is skipped), and `detect_binary` is a per-blob decision. See
///   [`crate::diff::DiffOptions`].
///
/// # Errors
///
/// [`crate::error::Error::ObjectNotFound`] for a tree or blob the database lacks,
/// [`crate::error::Error::WrongObjectKind`] for an id that is not what its entry's mode implies,
/// and the tree parser's validation errors for a malformed subtree.
pub fn tree_diff(
    format: ObjectFormat,
    old: ObjectId,
    new: ObjectId,
    odb: &Odb,
    options: &crate::diff::DiffOptions,
) -> Result<TreeDiff> {
    todo!()
}

/// Phase 8 — the diff between an index and a tree.
///
/// This is `git diff --staged` (and, with `tree` being the tree of `HEAD`, it is also the
/// comparison `status` shows as "to be committed").
///
/// # Caveats
///
/// * **The index may have conflict stages**, which have no single content; see the free function
///   of the same name in [`crate::diff`] for the rule. The short version: conflicted paths are
///   skipped, and a caller that wants them reports them from `status` instead.
/// * **`tree: None` means an unborn `HEAD`**, and every stage-0 entry is then an addition. That
///   is the state after `git init` and the first `add`, and it is a legal state rather than an
///   error.
/// * **The index's entries carry modes that are already normalised** (see
///   [`crate::worktree::add`]), so a mode-only change against a tree shows up here exactly as it
///   does between two trees — no extra normalisation step, and none wanted.
/// * **The order of the result follows the index**, which is already in the byte order trees use,
///   so the two diff entry points agree about the order of the same paths.
///
/// # Errors
///
/// As [`tree_diff`], plus [`crate::error::Error::CorruptIndex`] is *not* raised here — the index
/// was parsed before this was called; this only reads the entries.
pub fn index_diff(
    index: &Index,
    tree: Option<ObjectId>,
    odb: &Odb,
    options: &crate::diff::DiffOptions,
) -> Result<TreeDiff> {
    todo!()
}

/// Phase 8 — the diff between the work tree and the index.
///
/// # Caveats
///
/// * **Untracked files are not in the result.** A file the index does not know about is not a
///   change; `git diff` prints nothing for it, and `--no-index` / an explicit path is the way to
///   compare two arbitrary files. Conflating the two would make a diff of a repository with a
///   fresh build tree enormous.
/// * **The stat cache is both read and written.** An entry whose stat data matches and that is
///   not racy is skipped without reading the file; a racy or mismatching one is read and hashed,
///   and the fresh stat is stored. That is what makes the second `diff` in a row fast.
/// * **A file the index tracks but the work tree lacks is a deletion**, and a file whose type
///   changed (a symlink became a regular file, a gitlink became a directory) is a type change —
///   both are decided from `lstat`, so a symlink is never followed to decide what it is.
/// * **A path that is a directory in the work tree and a file in the index is a deletion of the
///   file, not an error.** `lstat` reports a directory; the index's entry no longer corresponds
///   to any file, and Git reports the file as deleted. A directory in the work tree that the
///   index has files under is simply recursed into.
/// * **The pathspec filters the walk**, unlike [`tree_diff`]'s post-filtering, so an excluded
///   directory is not descended into.
///
/// # Errors
///
/// [`crate::error::Error::BareRepository`] when there is no work tree,
/// [`crate::error::Error::IoAtPath`] for a file that exists and cannot be read (a permission
/// failure must not be reported as "the file is now empty"), and the object database's errors.
pub fn work_tree_diff(
    worktree: &Worktree,
    index: &Index,
    stat_cache: &mut StatCache,
    odb: &Odb,
    options: &crate::diff::DiffOptions,
) -> Result<TreeDiff> {
    todo!()
}

/// Phase 8 — the diff for one index entry against one work tree file.
///
/// The inner step of [`work_tree_diff`], exposed because `status`'s refresh and the CLI's
/// single-path `diff <path>` both need exactly this and a second implementation would be a second
/// set of rules about stat data, symlinks and binary detection.
///
/// # Caveats
///
/// * **`worktree_data` is the file's bytes, already read by the caller**, because the caller is
///   the one that knows whether the read was necessary — a stat match means it was not, and
///   making this function read would make that optimisation impossible. `None` means the file is
///   absent, which makes the result a deletion.
/// * **The index entry's id is not trusted as the new side's id.** A work tree file's id is the
///   hash of its bytes, and the entry's id is the *old* side; a caller that passed the entry's id
///   as the new one would produce a diff against itself.
/// * **The mode comes from the fresh `lstat`**, classified through `core.filemode`/`core.symlinks`,
///   not from the entry — otherwise a `chmod` in the work tree is invisible to `diff` while
///   `status` reports it, and the two disagree about the same file.
///
/// # Errors
///
/// [`crate::error::Error::ObjectNotFound`] if the entry's blob is missing from the database, and
/// [`crate::error::Error::NotImplemented`] for the histogram algorithm.
pub fn entry_diff(
    worktree: &Worktree,
    entry: &IndexEntry,
    worktree_data: Option<&[u8]>,
    odb: &Odb,
    options: &crate::diff::DiffOptions,
) -> Result<FileDiff> {
    todo!()
}
