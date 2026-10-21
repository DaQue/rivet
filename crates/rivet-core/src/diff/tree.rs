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
    /// For a detected rename, the similarity percentage (100 for an exact rename).
    pub similarity: Option<u32>,
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
        self.new_path
            .as_ref()
            .map(|p| BStr::new(&p[..]))
            .or_else(|| self.old_path.as_ref().map(|p| BStr::new(&p[..])))
            .unwrap_or(BStr::new(&[]))
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
        self.old_mode.is_some()
            && self.new_mode.is_some()
            && self.old_oid.as_ref() == self.new_oid.as_ref()
            && self.old_mode != self.new_mode
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
        // A change of file type (regular file, symlink, gitlink), not a `chmod`.
        match (self.old_mode, self.new_mode) {
            (Some(old), Some(new)) => old.to_u32() & 0o170000 != new.to_u32() & 0o170000,
            _ => false,
        }
    }

    /// Whether this entry is a detected rename.
    pub fn is_rename(&self) -> bool {
        self.similarity.is_some() && self.old_path != self.new_path
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
        DiffStat {
            path: self.path().to_owned(),
            added: self.hunks.iter().map(|h| h.added()).sum(),
            deleted: self.hunks.iter().map(|h| h.removed()).sum(),
            is_binary: self.is_binary,
        }
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
        self.files.len()
    }

    /// Phase 8 — the diff for one path, if it changed.
    ///
    /// # Caveats
    ///
    /// The lookup is linear over `files` and is meant for a caller checking one or two paths
    /// (a post-checkout hook, a test assertion), not for a caller iterating; the latter already
    /// has the slice.
    pub fn path(&self, path: &BStr) -> Option<&FileDiff> {
        self.files.iter().find(|f| f.path() == path)
    }

    /// Phase 8 — the total insertions and deletions across every file.
    ///
    /// # Caveats
    ///
    /// Binary files contribute nothing and are not counted as zero-line changes; the caller that
    /// needs to know how many were skipped reads [`FileDiff::is_binary`] from `files`.
    pub fn totals(&self) -> (usize, usize) {
        let (mut added, mut deleted) = (0usize, 0usize);
        for file in &self.files {
            if file.is_binary {
                continue;
            }
            for hunk in &file.hunks {
                added += hunk.added();
                deleted += hunk.removed();
            }
        }
        (added, deleted)
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
        let mut files = Vec::new();
        let mut stats = Vec::new();
        for (f, s) in self.files.drain(..).zip(self.stats.drain(..)) {
            if prefix.is_empty() || f.path().starts_with(prefix) {
                files.push(f);
                stats.push(s);
            }
        }
        self.files = files;
        self.stats = stats;
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
/// [`crate::error::Error::WrongObjectKind`] if an id names a tree, a commit or a tag. Both diff
/// algorithms are supported.
pub fn blob_diff(
    _format: ObjectFormat,
    path: &BStr,
    old: Option<(ObjectId, EntryMode)>,
    new: Option<(ObjectId, EntryMode)>,
    odb: &Odb,
    options: &crate::diff::DiffOptions,
) -> Result<FileDiff> {
    let old_oid = old.map(|(o, _)| o);
    let old_mode = old.map(|(_, m)| m);
    let new_oid = new.map(|(o, _)| o);
    let new_mode = new.map(|(_, m)| m);
    let is_new = old_oid.is_none();
    let is_deleted = new_oid.is_none();

    // A gitlink names a commit in another repository: comparing its content would mean fetching
    // that repository, so it is reported as a one-line change with no hunks.
    let gitlink = |mode: Option<EntryMode>| mode == Some(EntryMode::Commit);
    if gitlink(old_mode) || gitlink(new_mode) {
        return Ok(FileDiff {
            old_path: if is_new {
                None
            } else {
                Some(BString::from(path))
            },
            new_path: if is_deleted {
                None
            } else {
                Some(BString::from(path))
            },
            old_mode,
            new_mode,
            old_oid,
            new_oid,
            hunks: Vec::new(),
            is_binary: false,
            is_new,
            is_deleted,
            algorithm: options.algorithm,
            similarity: None,
        });
    }

    let read_blob = |oid: &ObjectId| -> Result<Vec<u8>> {
        Ok(odb
            .read_typed(oid, crate::object::ObjectKind::Blob)?
            .into_data())
    };
    let old_data = match old_oid {
        Some(oid) => read_blob(&oid)?,
        None => Vec::new(),
    };
    let new_data = match new_oid {
        Some(oid) => read_blob(&oid)?,
        None => Vec::new(),
    };

    assemble_bytes(path, old, new, &old_data, &new_data, options)
}

/// The tail shared by every comparison: given both sides' identities *and* their bytes, build the
/// [`FileDiff`] — binary detection, the edit script, the hunks.
///
/// [`blob_diff`] passes bytes read from the object database, [`entry_diff`] passes a work tree
/// file's bytes, and the two must produce the same diff for the same bytes; splitting the tail out
/// is what keeps that true instead of being two implementations that drift.
fn assemble_bytes(
    path: &BStr,
    old: Option<(ObjectId, EntryMode)>,
    new: Option<(ObjectId, EntryMode)>,
    old_data: &[u8],
    new_data: &[u8],
    options: &crate::diff::DiffOptions,
) -> Result<FileDiff> {
    let old_oid = old.map(|(o, _)| o);
    let old_mode = old.map(|(_, m)| m);
    let new_oid = new.map(|(o, _)| o);
    let new_mode = new.map(|(_, m)| m);
    let is_new = old_oid.is_none();
    let is_deleted = new_oid.is_none();

    let is_binary = options.detect_binary
        && (crate::diff::is_binary(old_data) || crate::diff::is_binary(new_data));

    let hunks = if is_binary || old_oid == new_oid {
        Vec::new()
    } else {
        compute_hunks(old_data, new_data, options)?
    };

    Ok(FileDiff {
        old_path: if is_new {
            None
        } else {
            Some(BString::from(path))
        },
        new_path: if is_deleted {
            None
        } else {
            Some(BString::from(path))
        },
        old_mode,
        new_mode,
        old_oid,
        new_oid,
        hunks,
        is_binary,
        is_new,
        is_deleted,
        algorithm: options.algorithm,
        similarity: None,
    })
}

/// The edit script and hunks for two byte strings.
///
/// Returns the binary flag alongside the hunks because the two decisions are made in one place:
/// a file detected as binary must have *no* hunks, and returning `(hunks, is_binary)` separately
/// from two functions would let a caller print hunk text for a file it then labels binary.
fn compute_hunks(
    old_data: &[u8],
    new_data: &[u8],
    options: &crate::diff::DiffOptions,
) -> Result<Vec<Hunk>> {
    use crate::diff::myers::{histogram_diff, split_lines};

    let old_lines = split_lines(old_data);
    let new_lines = split_lines(new_data);
    let edits = match options.algorithm {
        crate::diff::DiffAlgorithm::Myers => crate::diff::myers::git_diff(&old_lines, &new_lines),
        crate::diff::DiffAlgorithm::Histogram => histogram_diff(&old_lines, &new_lines)?,
    };
    let edits = crate::diff::myers::compact_edits(&old_lines, &new_lines, &edits);
    Ok(crate::diff::unified::build_hunks(
        old_data,
        new_data,
        &edits,
        options.context,
    ))
}

fn tree_diff_from(files: Vec<FileDiff>) -> TreeDiff {
    let stats = files.iter().map(FileDiff::stat).collect();
    TreeDiff { files, stats }
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
    let mut files = Vec::new();
    walk_trees(
        format,
        Some(&old),
        Some(&new),
        BString::new(Vec::new()),
        odb,
        options,
        &mut files,
    )?;
    Ok(tree_diff_from(files))
}

/// The parallel walk behind [`tree_diff`].
///
/// Either side may be absent, which is how a subtree that exists on only one side is flattened:
/// every entry under it becomes an addition or a deletion. `prefix` is the directory path of the
/// two trees, already ending in `/` when non-empty.
fn walk_trees(
    format: ObjectFormat,
    old: Option<&ObjectId>,
    new: Option<&ObjectId>,
    prefix: BString,
    odb: &Odb,
    options: &crate::diff::DiffOptions,
    out: &mut Vec<FileDiff>,
) -> Result<()> {
    use crate::object::TreeEntry;

    let read_entries = |oid: &ObjectId| -> Result<Vec<TreeEntry>> {
        // The empty tree is a well-known id Git never needs on disk (`git diff 4b825dc`);
        // treating it as "no entries" saves every root-commit diff from writing it.
        if *oid == crate::object::hash_object(format, crate::object::ObjectKind::Tree, b"")?.0 {
            return Ok(Vec::new());
        }
        let data = odb
            .read_typed(oid, crate::object::ObjectKind::Tree)?
            .into_data();
        Ok(crate::object::Tree::parse(format, &data)?.entries)
    };

    let old_entries = match old {
        Some(oid) => read_entries(oid)?,
        None => Vec::new(),
    };
    let new_entries = match new {
        Some(oid) => read_entries(oid)?,
        None => Vec::new(),
    };

    let mut oi = 0usize;
    let mut ni = 0usize;
    while oi < old_entries.len() || ni < new_entries.len() {
        let ordering = match (old_entries.get(oi), new_entries.get(ni)) {
            (Some(a), Some(b)) => tree_entry_cmp(a, b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => break,
        };

        match ordering {
            std::cmp::Ordering::Less => {
                let entry = &old_entries[oi];
                oi += 1;
                if entry.mode == EntryMode::Tree {
                    walk_trees(
                        format,
                        Some(&entry.oid),
                        None,
                        child_prefix(BStr::new(&prefix[..]), BStr::new(&entry.name[..])),
                        odb,
                        options,
                        out,
                    )?;
                } else {
                    let path = child_path(BStr::new(&prefix[..]), BStr::new(&entry.name[..]));
                    if in_pathspec(BStr::new(&path[..]), BStr::new(&options.pathspec[..])) {
                        out.push(blob_diff(
                            format,
                            BStr::new(&path[..]),
                            Some((entry.oid, entry.mode)),
                            None,
                            odb,
                            options,
                        )?);
                    }
                }
            }
            std::cmp::Ordering::Greater => {
                let entry = &new_entries[ni];
                ni += 1;
                if entry.mode == EntryMode::Tree {
                    walk_trees(
                        format,
                        None,
                        Some(&entry.oid),
                        child_prefix(BStr::new(&prefix[..]), BStr::new(&entry.name[..])),
                        odb,
                        options,
                        out,
                    )?;
                } else {
                    let path = child_path(BStr::new(&prefix[..]), BStr::new(&entry.name[..]));
                    if in_pathspec(BStr::new(&path[..]), BStr::new(&options.pathspec[..])) {
                        out.push(blob_diff(
                            format,
                            BStr::new(&path[..]),
                            None,
                            Some((entry.oid, entry.mode)),
                            odb,
                            options,
                        )?);
                    }
                }
            }
            std::cmp::Ordering::Equal => {
                let a = &old_entries[oi];
                let b = &new_entries[ni];
                oi += 1;
                ni += 1;
                if a.mode == EntryMode::Tree && b.mode == EntryMode::Tree {
                    if a.oid != b.oid {
                        walk_trees(
                            format,
                            Some(&a.oid),
                            Some(&b.oid),
                            child_prefix(BStr::new(&prefix[..]), BStr::new(&a.name[..])),
                            odb,
                            options,
                            out,
                        )?;
                    }
                } else if a.oid != b.oid || a.mode != b.mode {
                    let path = child_path(BStr::new(&prefix[..]), BStr::new(&a.name[..]));
                    if in_pathspec(BStr::new(&path[..]), BStr::new(&options.pathspec[..])) {
                        out.push(blob_diff(
                            format,
                            BStr::new(&path[..]),
                            Some((a.oid, a.mode)),
                            Some((b.oid, b.mode)),
                            odb,
                            options,
                        )?);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Git's tree ordering: a directory's name sorts as if it ended in `/`.
fn tree_entry_cmp(
    a: &crate::object::TreeEntry,
    b: &crate::object::TreeEntry,
) -> std::cmp::Ordering {
    fn key(name: &BStr, mode: EntryMode) -> Vec<u8> {
        let mut key = name.to_vec();
        if mode == EntryMode::Tree {
            key.push(b'/');
        }
        key
    }
    key(BStr::new(&a.name[..]), a.mode).cmp(&key(BStr::new(&b.name[..]), b.mode))
}

fn child_prefix(prefix: &BStr, name: &BStr) -> BString {
    let mut path = BString::from(prefix);
    path.extend_from_slice(name);
    path.push(b'/');
    path
}

/// A file's full path: the directory prefix (which ends in `/` when non-empty) plus its name.
fn child_path(prefix: &BStr, name: &BStr) -> BString {
    let mut path = BString::from(prefix);
    path.extend_from_slice(name);
    path
}

fn in_pathspec(path: &BStr, spec: &BStr) -> bool {
    if spec.is_empty() {
        return true;
    }
    let path: &[u8] = path.as_ref();
    let spec: &[u8] = spec.as_ref();
    if !path.starts_with(spec) {
        return false;
    }
    path.len() == spec.len() || spec.ends_with(b"/") || path[spec.len()] == b'/'
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
    use crate::object::ObjectKind;

    // The comparison is *staged*: index against `HEAD`'s tree. Doing it the other way round —
    // walking `HEAD` and asking the index about each path — would miss nothing, but it would
    // report the paths in the tree's order and re-read the index for every one of them; the
    // status code has already solved this comparison once, so this reuses its rules.
    let changes = crate::worktree::status::staged_changes(index, tree, odb)?;
    let mut files = Vec::with_capacity(changes.len());
    for change in changes {
        if !in_pathspec(
            BStr::new(&change.path[..]),
            BStr::new(&options.pathspec[..]),
        ) {
            continue;
        }
        // A conflicted path has three contents and none of them is "the" staged side; `status`
        // reports it as unmerged and `diff` skips it (see the caveats above).
        if matches!(change.kind, crate::worktree::status::ChangeKind::Unmerged) {
            continue;
        }
        let old_data = match change.old_oid {
            Some(oid) => odb.read_typed(&oid, ObjectKind::Blob)?.into_data(),
            None => Vec::new(),
        };
        let new_data = match change.new_oid {
            Some(oid) => odb.read_typed(&oid, ObjectKind::Blob)?.into_data(),
            None => Vec::new(),
        };
        files.push(assemble_bytes(
            BStr::new(&change.path[..]),
            change.old_oid.zip(change.old_mode),
            change.new_oid.zip(change.new_mode),
            &old_data,
            &new_data,
            options,
        )?);
    }
    Ok(tree_diff_from(files))
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
    // `unstaged_changes` already owns the two decisions that are easy to get wrong here: whether
    // the stat cache lets the file be skipped without reading it, and what the file's mode is
    // after a fresh `lstat`. Re-deciding either in the diff would let `status` and `diff`
    // disagree about the same file, which is exactly the class of bug this module exists to
    // avoid — so the scan runs once and the bytes are read only for the paths it reports.
    let changes =
        crate::worktree::status::unstaged_changes(worktree, index, odb, false, stat_cache)?;
    let mut files = Vec::with_capacity(changes.len());
    for change in changes {
        if !in_pathspec(
            BStr::new(&change.path[..]),
            BStr::new(&options.pathspec[..]),
        ) {
            continue;
        }
        let new_data = match change.new_oid {
            Some(_) => match worktree.read_file(BStr::new(&change.path[..])) {
                Ok(data) => Some(data),
                // The scan saw the file a moment ago; if it is gone now the honest reading is a
                // deletion, which is also what a fresh scan would report.
                Err(crate::error::Error::IoAtPath { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(error) => return Err(error),
            },
            None => None,
        };
        let old_data = match change.old_oid {
            Some(oid) => odb
                .read_typed(&oid, crate::object::ObjectKind::Blob)?
                .into_data(),
            None => Vec::new(),
        };
        // The right-hand identity is dropped when the bytes could not be read, so that a file
        // that vanished between the scan and the read is a deletion rather than an empty file.
        let new = match new_data {
            Some(_) => change.new_oid.zip(change.new_mode),
            None => None,
        };
        files.push(assemble_bytes(
            BStr::new(&change.path[..]),
            change.old_oid.zip(change.old_mode),
            new,
            &old_data,
            new_data.as_deref().unwrap_or(&[]),
            options,
        )?);
    }
    Ok(tree_diff_from(files))
}

/// Phase 8 — the diff for one index entry against one work tree file.
///
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
/// [`crate::error::Error::ObjectNotFound`] if the entry's blob is missing from the database.
pub fn entry_diff(
    worktree: &Worktree,
    entry: &IndexEntry,
    worktree_data: Option<&[u8]>,
    odb: &Odb,
    options: &crate::diff::DiffOptions,
) -> Result<FileDiff> {
    let old = Some((entry.oid, entry.mode));

    let new = match worktree_data {
        Some(data) => {
            // The new id is the hash of the bytes the caller read, never the entry's id: the
            // entry describes the *index* side, and reusing it would diff the file against
            // itself. Hashing here is what makes a `chmod`-only change fall through
            // `assemble_bytes`' `old_oid == new_oid` shortcut and report a mode change with no
            // hunks, exactly as Git does.
            let id = crate::object::Object::Blob(data.to_vec()).id(worktree.format())?;
            let mode = match worktree.stat(BStr::new(&entry.path[..]))? {
                Some(stat) => stat.mode,
                // The bytes were readable a moment ago and the file is gone now: report the
                // deletion rather than an empty file with the index's mode.
                None => {
                    return assemble_bytes(
                        BStr::new(&entry.path[..]),
                        old,
                        None,
                        &[],
                        &[],
                        options,
                    );
                }
            };
            Some((id, mode))
        }
        None => None,
    };

    let old_data = odb
        .read_typed(&entry.oid, crate::object::ObjectKind::Blob)?
        .into_data();
    assemble_bytes(
        BStr::new(&entry.path[..]),
        old,
        new,
        &old_data,
        worktree_data.unwrap_or(&[]),
        options,
    )
}
