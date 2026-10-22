//! Phase 4 — `status`: what differs between `HEAD`, the index and the work tree.
//!
//! `status` is three comparisons, and the CLI prints them as two lists because two of the three
//! are about the same pair of things:
//!
//! ```text
//! HEAD ──(1)── index ──(2)── work tree
//! ```
//!
//! * **(1) is "staged"** — `Changes to be committed`, the diff between `HEAD`'s tree and the
//!   index. A path here is what `commit` would record.
//! * **(2) is "not staged"** — `Changes not staged for commit`, the diff between the index and
//!   the work tree. A path here is what `add` would pick up.
//! * **A path in neither, and not in the index, is "untracked"** — unless an ignore rule
//!   matched it, in which case it is not reported at all.
//!
//! # Why the order of comparison matters
//!
//! A path can be in all three states at once, and the reporting rules are not symmetric:
//!
//! * **Modified in the index and in the work tree** reports twice — once staged, once unstaged —
//!   and it is *two* entries in the output, not one with two flags. `git status --porcelain`
//!   prints `MM`, and a caller that collapses them loses the distinction between "staged the
//!   change" and "staged one change and then made another".
//! * **Deleted in the work tree but present in the index** is the "not staged: deleted" case.
//!   It is *not* reported as untracked, which is the bug a naive "walk the work tree and look up
//!   each path in the index" implementation produces for every file it does not find.
//! * **Present in `HEAD`'s tree and absent from the index** is a staged deletion. It is found by
//!   walking the tree, not the work tree, and a status that only walks the work tree misses it
//!   entirely — which is the failure mode where `git rm` then `status` shows nothing.
//!
//! # Caveats that bite
//!
//! * **The racy-git rule decides whether a file is read.** An index entry whose mtime is not
//!   strictly older than the index's own is re-hashed rather than trusted (see
//!   [`crate::index::Index::is_racy`]); all others are compared by stat alone. That is what
//!   makes `status` on a large repository fast, and it is also why a file changed within the
//!   same clock tick that wrote the index must be re-read rather than believed.
//! * **`assume-valid` entries are skipped entirely.** Git trusts the user's promise that the
//!   file has not changed, so the entry is reported neither as modified nor as deleted, even
//!   when it is one. `skip-worktree` entries are likewise absent from the work tree and are not
//!   reported as deleted.
//! * **A mode-only change is a modification.** `100644` to `100755` is [`ChangeKind::Modified`]
//!   with the same blob id on both sides, exactly as `git status` reports it, so a comparison
//!   that only looked at ids would see no change at all. [`ChangeKind::TypeChanged`] is for a
//!   change of file type (regular file, symlink, gitlink).
//! * **An untracked directory is reported as one entry, not per file.** Git prints
//!   `?? dir/` and stops, because a build directory with 100 000 files is not a useful listing;
//!   `--untracked-files=all` is the form that descends, and it is a caller-side choice.
//! * **A submodule is a gitlink and is never descended into.** Its work tree is another
//!   repository; reporting its files as untracked would list a nested repository's contents.
//! * **Nothing here writes.** A status that updates the index's stat cache is a status that
//!   changes the repository, and Git's own does so only under `--refresh`; the signature has no
//!   `&mut Index` for that reason.

use std::collections::{BTreeMap, BTreeSet};

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::ignore::IgnoreStack;
use crate::index::Index;
use crate::odb::Odb;
use crate::worktree::Worktree;

/// Phase 4 — what kind of change a status entry reports.
///
/// # Caveats
///
/// * **The variant order is the order the CLI sorts by, not a severity.** `git status` prints
///   additions, then modifications, then deletions, then renames and type changes, and the
///   differential tests compare that ordering.
/// * **`TypeChanged` is a change of file type** — regular file, symlink or gitlink — and
///   not a mode-only change such as `100644` to `100755`, which Git reports as a modification.
/// * **There is no `Renamed` variant.** Rename detection is a similarity guess, not a fact about
///   the repository, and it is explicitly a later phase (plan §4 Phase 8). A rename appears as a
///   deletion and an addition, which is exactly what `git status --no-renames` prints.
/// * **`Unmerged` is a state, not a difference.** A conflicted path has no single content to
///   compare, so it is reported here and the per-stage detail is the index's stages, not this
///   enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChangeKind {
    /// The path exists on the right-hand side but not the left.
    Added,
    /// The path exists on both sides with different content.
    Modified,
    /// The path exists on the left-hand side but not the right.
    Deleted,
    /// The path exists on both sides with the same content but a different mode — or a different
    /// object kind, such as a file replaced by a symlink.
    TypeChanged,
    /// The path has conflict stages in the index and cannot be compared.
    Unmerged,
}

impl ChangeKind {
    /// Phase 4 — the letter `git status --porcelain` prints for this change.
    ///
    /// # Caveats
    ///
    /// `A`, `M`, `D`, `T` and `U`. For an unmerged path Git prints the pair of letters describing
    /// which stages are present (`DD`, `AU`, `UA`, `UU`, …) rather than a bare `U`, and this is
    /// only the first letter — a caller that needs Git's full unmerged code has to look at the
    /// index's stages itself.
    pub const fn code(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
            ChangeKind::TypeChanged => 'T',
            ChangeKind::Unmerged => 'U',
        }
    }
}

/// Phase 4 — one path's change, in one of the three comparisons.
///
/// # Caveats
///
/// * **The same path appears up to twice in a [`Status`]**, once as a staged change and once as
///   an unstaged one. They are separate entries because they are separate facts — this is what
///   `--porcelain` renders as the two characters of `XY`.
/// * **`old_oid` and `new_oid` are `None` on the side where the path does not exist.** For an
///   addition the old side is absent, for a deletion the new side is, and neither is ever the
///   null id — a zero id means "the ref was created" in the wire protocol and reusing it here
///   would make an addition indistinguishable from a file emptied to nothing.
/// * **The ids may be the same and the modes still differ** (a `chmod`), so a caller rendering a
///   change must not derive the kind from the ids alone.
/// * **`path` is the work-tree-relative path as bytes.** It is the bytes of the index entry or
///   the tree entry, never a re-render, because a path that is not UTF-8 has no faithful
///   `String` form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    /// The path, relative to the work tree root, `/`-separated.
    pub path: BString,
    /// What kind of change this is.
    pub kind: ChangeKind,
    /// The id on the left-hand side, absent when the path does not exist there.
    pub old_oid: Option<ObjectId>,
    /// The id on the right-hand side, absent when the path does not exist there.
    pub new_oid: Option<ObjectId>,
    /// The mode on the left-hand side, when there is one — from `HEAD`'s tree or the index
    /// depending on which comparison this entry came from.
    pub old_mode: Option<crate::object::EntryMode>,
    /// The mode on the right-hand side, when there is one.
    pub new_mode: Option<crate::object::EntryMode>,
}

/// Phase 4 — the whole comparison, as `status` prints it.
///
/// # Caveats
///
/// * **The three lists are disjoint in *meaning* and not in *path*.** A path can appear in
///   `staged` and in `unstaged` at once; it cannot appear in `untracked` while being in the
///   index at all.
/// * **`untracked` holds directories as well as files** unless the caller asked for the fully
///   expanded form, so a caller counting files must not treat its length as a file count.
/// * **All three are sorted by path bytes**, which is the order `git status` prints in a
///   repository with no `status.relativePaths` surprises. Sorting is part of building the
///   struct, not of rendering it, so that `--porcelain` and the human form agree.
/// * **The `HeadState` is not here.** Whether the branch is unborn, attached or detached is a
///   refs question, and a caller composing the printed status takes it from
///   [`crate::refs::RefStore::head`] — which is also the only way to know that `head_tree` was
///   `None` because there are no commits rather than because the tree was missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Paths whose index entry differs from `HEAD` — what `commit` would record.
    pub staged: Vec<StatusEntry>,
    /// Paths whose work tree file differs from the index — what `add` would pick up.
    pub unstaged: Vec<StatusEntry>,
    /// Paths in the work tree that the index does not know about and no ignore rule matched.
    pub untracked: Vec<BString>,
    /// Paths in the work tree that the index does not know about and an ignore rule matched.
    ///
    /// # Caveats
    ///
    /// Empty unless the caller asked for ignored entries, which is what `status --ignored`
    /// does. A caller that always populated it would make every plain status descend into the
    /// directories ignore rules exist to skip.
    pub ignored: Vec<BString>,
}

impl Status {
    /// Phase 4 — whether the work tree, the index and `HEAD` all agree.
    ///
    /// # Caveats
    ///
    /// **Ignored files do not count.** A repository whose only entry is in `ignored` is clean,
    /// which is the whole point of an ignore rule — `git status` prints "nothing to commit,
    /// working tree clean" while a build directory sits there.
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }

    /// Phase 4 — how many paths are reported anywhere, ignored entries excluded.
    ///
    /// # Caveats
    ///
    /// A count of **entries**, so a path that is both staged and unstaged is counted twice. That
    /// is what `git status --porcelain` lines number, and it is not the number of files involved.
    pub fn count(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.untracked.len()
    }

    /// Phase 4 — every conflicted path, from the staged list.
    ///
    /// The list `commit` refuses on and `merge` reports, in the form the user sees it.
    ///
    /// # Caveats
    ///
    /// A conflicted path appears in `staged` with [`ChangeKind::Unmerged`] and never in
    /// `unstaged`: its work tree content is the conflict-marked file `merge` wrote, and
    /// comparing that against the index would report a modification the user did not make.
    pub fn conflicts(&self) -> Vec<&BString> {
        self.staged
            .iter()
            .filter(|e| e.kind == ChangeKind::Unmerged)
            .map(|e| &e.path)
            .collect()
    }
}

/// Phase 4 — how much of the work tree a status should collect.
///
/// # Caveats
///
/// * **`untracked_files` is a three-way choice, not a boolean.** Git's
///   `--untracked-files=no|normal|all` maps to "do not collect", "collect directories as one
///   entry" and "descend and collect every file", and the difference is visible in the output
///   the moment a nested untracked directory exists.
/// * **`refresh` does not mean "update the index".** It controls whether Rivet re-hashes a file
///   whose stat data says it is unchanged; the index is never written by this module.
/// * **`pathspec` is a filter applied after the comparison, not before the walk.** Filtering the
///   walk would be faster and would change which untracked directories are reported, because a
///   directory is only known to be entirely untracked after looking at all of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusOptions {
    /// How much of the untracked part of the work tree to report.
    pub untracked_files: UntrackedMode,
    /// Whether to report ignored paths in their own list.
    pub ignored: bool,
    /// Whether to re-hash entries whose stat data claims they are unchanged.
    pub refresh: bool,
    /// An optional path prefix to limit the report to, empty for everything.
    pub pathspec: BString,
}

/// Phase 4 — the three settings of Git's `--untracked-files`.
///
/// # Caveats
///
/// The middle variant is the default and the one that is easy to get wrong: `normal` reports an
/// untracked *directory* as a single entry with a trailing `/`, and a caller that treated that
/// as a file path would fail to find it on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UntrackedMode {
    /// `--untracked-files=no` — report nothing untracked.
    No,
    /// `--untracked-files=normal` — one entry per untracked directory, files at the top level.
    Normal,
    /// `--untracked-files=all` — descend and report every untracked file.
    All,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusOptions {
    /// Phase 4 — the options a plain `rivet status` uses.
    ///
    /// **`Normal` untracked reporting, no ignored list, no forced refresh**, which is Git's own
    /// default and the one the differential tests compare against.
    pub fn new() -> Self {
        Self {
            untracked_files: UntrackedMode::Normal,
            ignored: false,
            refresh: false,
            pathspec: BString::from(""),
        }
    }

    /// Phase 4 — set the untracked mode.
    pub fn untracked(&mut self, mode: UntrackedMode) -> &mut Self {
        self.untracked_files = mode;
        self
    }

    /// Phase 4 — set whether ignored paths are collected.
    pub fn ignored(&mut self, on: bool) -> &mut Self {
        self.ignored = on;
        self
    }

    /// Phase 4 — limit the report to a path prefix.
    pub fn pathspec(&mut self, prefix: &bstr::BStr) -> &mut Self {
        self.pathspec = prefix.to_owned();
        self
    }
}

/// Phase 4 — compute a [`Status`].
///
/// The algorithm, in the order it must run:
///
/// 1. **Compare `HEAD`'s tree against the index**, walking the tree to find staged deletions
///    and the index to find staged additions, and pairing the rest by path. This produces
///    `staged`.
/// 2. **Compare the index against the work tree.** Each stage-0 entry is looked up on disk; a
///    missing file is a deletion, a changed stat is a modification, and a mode difference is
///    [`ChangeKind::TypeChanged`]. Conflicted paths are reported as
///    [`ChangeKind::Unmerged`] and skipped.
/// 3. **Walk the work tree** for paths the index does not know about, applying ignore rules and
///    the untracked mode. This produces `untracked` (and `ignored`, if asked for).
///
/// # Caveats
///
/// * **`head_tree` is `Option` because a branch can be unborn.** `None` means every index entry
///   is an addition, which is what `git status` shows on a fresh repository.
/// * **The odb is needed for step 1 and for nothing else.** Reconstructing `HEAD`'s tree means
///   reading trees, and the tree's entries are what a staged deletion is found by walking.
/// * **This does not read or write refs and does not touch the index file.** The caller passes
///   both in; `repo::Repository::status` is what pairs them up.
/// * **The racy rule makes step 2 read files.** An entry that is racy — see
///   [`crate::index::Index::is_racy`] — is hashed and compared by id rather than by stat, and
///   that is a read of the file, not a comparison of numbers.
/// * **Ignore rules apply to `untracked` only.** A tracked file that an ignore rule matches is
///   still reported when it changes, because ignore rules stop a file becoming tracked and do
///   nothing once it is.
///
/// # Errors
///
/// [`crate::error::Error::BareRepository`] when there is no work tree;
/// [`crate::error::Error::ObjectNotFound`] or the tree-parse errors of the object database for
/// a `head_tree` that does not resolve; [`crate::error::Error::IoAtPath`] for a directory that
/// cannot be read during the untracked walk; and [`crate::error::Error::InvalidPath`] for a
/// pathspec or an index entry that escapes the work tree.
pub fn compute(
    worktree: &Worktree,
    index: &Index,
    head_tree: Option<ObjectId>,
    odb: &Odb,
    ignore: &IgnoreStack,
    options: &StatusOptions,
) -> Result<Status> {
    let staged = staged_changes(index, head_tree, odb)?;
    let mut stat_cache = crate::worktree::StatCache::new();
    let unstaged = unstaged_changes(worktree, index, odb, options.refresh, &mut stat_cache)?;
    let (untracked, ignored) = untracked_paths(worktree, index, ignore, options)?;
    Ok(Status {
        staged,
        unstaged,
        untracked,
        ignored,
    })
}

/// Phase 4 — the difference between `HEAD`'s tree and the index.
///
/// Split out from [`compute`] because `git diff --cached` and the `commit` dry run ask exactly
/// this question and would otherwise build a whole work tree comparison to answer it.
///
/// # Caveats
///
/// * **Conflicted paths are reported as [`ChangeKind::Unmerged`]** rather than being compared,
///   because there is no stage-0 entry to compare with and picking a stage would invent a
///   resolution.
/// * **`head_tree: None` makes every index entry an addition**, including the ones whose id
///   equals the empty blob — an empty file added to an empty repository is a real addition.
/// * **The tree walk is what finds deletions.** A path present in the tree and absent from the
///   index is a staged deletion, and it is invisible to any comparison driven by the index's
///   entry list.
///
/// # Errors
///
/// The object database's read errors for `head_tree`'s subtrees, and
/// [`crate::error::Error::WrongObjectKind`] if a tree entry's id is not the tree it claims to be.
pub fn staged_changes(
    index: &Index,
    head_tree: Option<ObjectId>,
    odb: &Odb,
) -> Result<Vec<StatusEntry>> {
    // The tree side is collected first, because the index can only tell us about additions:
    // a path that HEAD has and the index no longer does is invisible from the index's list.
    let mut tracked: BTreeMap<BString, (crate::object::EntryMode, ObjectId)> = BTreeMap::new();
    if let Some(tree_id) = head_tree {
        let mut prefix = Vec::new();
        collect_tree_entries(odb, odb.format(), tree_id, &mut prefix, &mut tracked)?;
    }

    let mut out = Vec::new();
    let mut conflicted: BTreeSet<BString> = BTreeSet::new();

    for entry in index.entries() {
        if entry.flags.stage()? != crate::index::Stage::Normal {
            conflicted.insert(entry.path.clone());
            continue;
        }
        match tracked.remove(&entry.path) {
            None => out.push(StatusEntry {
                path: entry.path.clone(),
                kind: ChangeKind::Added,
                old_oid: None,
                new_oid: Some(entry.oid),
                old_mode: None,
                new_mode: Some(entry.mode),
            }),
            Some((mode, oid)) => {
                if oid == entry.oid && mode == entry.mode {
                    continue;
                }
                // A change of object type, or a mode-only change with the same blob id, is a
                // type change: the content is identical and an id-only comparison would miss it.
                let kind = if file_type(mode) != file_type(entry.mode) {
                    ChangeKind::TypeChanged
                } else {
                    ChangeKind::Modified
                };
                out.push(StatusEntry {
                    path: entry.path.clone(),
                    kind,
                    old_oid: Some(oid),
                    new_oid: Some(entry.oid),
                    old_mode: Some(mode),
                    new_mode: Some(entry.mode),
                });
            }
        }
    }

    // Whatever HEAD still has was removed from the index. A path that is conflicted is not a
    // deletion: it has no stage 0 to compare and is reported as unmerged instead.
    for (path, (mode, oid)) in tracked {
        if conflicted.contains(&path) {
            continue;
        }
        out.push(StatusEntry {
            path,
            kind: ChangeKind::Deleted,
            old_oid: Some(oid),
            new_oid: None,
            old_mode: Some(mode),
            new_mode: None,
        });
    }

    for path in conflicted {
        out.push(StatusEntry {
            path,
            kind: ChangeKind::Unmerged,
            old_oid: None,
            new_oid: None,
            old_mode: None,
            new_mode: None,
        });
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Walk one tree recursively, recording every non-tree entry by its full path.
///
/// Gitlinks are recorded rather than descended into: a submodule's tree lives in another
/// repository and reading it here would report another repository's state as this one's.
fn collect_tree_entries(
    odb: &Odb,
    format: crate::hash::ObjectFormat,
    tree_id: ObjectId,
    prefix: &mut Vec<u8>,
    out: &mut BTreeMap<BString, (crate::object::EntryMode, ObjectId)>,
) -> Result<()> {
    let object = odb.read(&tree_id)?;
    if object.kind() != crate::object::ObjectKind::Tree {
        return Err(crate::error::Error::WrongObjectKind {
            oid: Some(tree_id),
            expected: crate::object::ObjectKind::Tree,
            found: object.kind(),
        });
    }
    let tree = crate::object::Tree::parse(format, object.data())?;
    for item in tree.iter() {
        let len = prefix.len();
        if len > 0 {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(item.name.as_ref());
        if file_type(item.mode) == 0o040000 {
            collect_tree_entries(odb, format, item.oid, prefix, out)?;
        } else {
            out.insert(BString::from(prefix.as_slice()), (item.mode, item.oid));
        }
        prefix.truncate(len);
    }
    Ok(())
}

/// The object type encoded in a mode's high bits: regular, directory, symlink or gitlink.
fn file_type(mode: crate::object::EntryMode) -> u32 {
    mode.to_u32() & 0o170000
}

/// Phase 4 — the difference between the index and the work tree.
///
/// The second half of [`compute`], exposed because `add -n` and `diff` ask it without wanting
/// the untracked walk.
///
/// # Caveats
///
/// * **`stat_cache` is what makes this not read every file.** An entry whose stat data matches
///   its file and is not racy is taken as unchanged; everything else is read and hashed, and
///   the results are handed back so a caller that writes the index can refresh the cache.
/// * **A missing file is a deletion, not an error.** A file that exists and cannot be read is an
///   error, and the two must not be folded together — a permission error reported as "deleted"
///   is how a `commit -a` loses a file.
/// * **Symlinks are compared by target**, never followed; see
///   [`crate::worktree::Worktree::read_file`].
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`], [`crate::error::Error::BareRepository`], and
/// [`crate::error::Error::InvalidPath`] for an index entry that escapes the work tree.
pub fn unstaged_changes(
    worktree: &Worktree,
    index: &Index,
    _odb: &Odb,
    refresh: bool,
    stat_cache: &mut crate::worktree::StatCache,
) -> Result<Vec<StatusEntry>> {
    let index_mtime = worktree.index_mtime();
    let mut out = Vec::new();

    for entry in index.entries() {
        // Conflicted paths were reported by `staged_changes`; there is no work-tree side to
        // compare them against until the conflict is resolved.
        if entry.flags.stage()? != crate::index::Stage::Normal {
            continue;
        }
        // A gitlink's work-tree side is another repository. Comparing it here would report that
        // repository's state as a modification of this one.
        if matches!(entry.mode, crate::object::EntryMode::Commit) {
            continue;
        }
        // Both flags mean "do not look at this file": assume-valid is the user's promise and
        // skip-worktree is the sparse-checkout machinery's. Git reports neither.
        let extended = crate::index::IndexEntryExtendedFlags::from_bits(entry.extended_flags);
        if entry.flags.assume_valid() || extended.skip_worktree() {
            continue;
        }

        let cached = stat_cache.stat(worktree, BStr::new(&entry.path))?.copied();
        let Some(data) = cached else {
            // The file is gone. A file that exists but cannot be read surfaces as an error from
            // `stat`, above, and must not be folded into "deleted".
            out.push(StatusEntry {
                path: entry.path.clone(),
                kind: ChangeKind::Deleted,
                old_oid: Some(entry.oid),
                new_oid: None,
                old_mode: Some(entry.mode),
                new_mode: None,
            });
            continue;
        };

        // A stat match is only trustworthy when it is not racy: an entry written in the same
        // timestamp tick as the index could have been modified again without the mtime moving.
        // `--refresh` asks to distrust every stat match and read the file regardless.
        let racy = refresh || index.is_racy(entry, index_mtime);
        if !racy && data.is_unchanged(entry, index_mtime) {
            continue;
        }

        let mode = data.mode;
        let new_oid = worktree.hash_file(BStr::new(&entry.path))?;
        if new_oid == entry.oid && mode == entry.mode {
            continue;
        }
        // The repository's own convention: a mode change is a type change even when the blob id
        // is unchanged, because an id-only comparison cannot see it at all.
        let kind = if file_type(mode) != file_type(entry.mode) {
            ChangeKind::TypeChanged
        } else {
            ChangeKind::Modified
        };
        out.push(StatusEntry {
            path: entry.path.clone(),
            kind,
            old_oid: Some(entry.oid),
            new_oid: Some(new_oid),
            old_mode: Some(entry.mode),
            new_mode: Some(mode),
        });
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Phase 4 — the paths in the work tree that the index does not know about.
///
/// The third half of [`compute`], and the only one that consults ignore rules.
///
/// # Caveats
///
/// * **A directory containing only ignored files is itself untracked and is reported.** Git does
///   not report a directory as ignored unless the directory itself matched a rule; content is
///   not what makes a directory ignored.
/// * **An empty directory is invisible.** Git does not track directories, so an empty one appears
///   here as an untracked directory in `Normal` mode and as nothing in `All` mode, because there
///   are no files under it to report.
/// * **The result is sorted by path bytes** and deduplicated, so that two ways of reaching the
///   same path cannot produce two entries.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] for a directory that cannot be read, and
/// [`crate::error::Error::InvalidPath`] for a path the walk produced that escapes the root —
/// which can only happen if the filesystem has been modified under the walk.
pub fn untracked_paths(
    worktree: &Worktree,
    index: &Index,
    ignore: &IgnoreStack,
    options: &StatusOptions,
) -> Result<(Vec<BString>, Vec<BString>)> {
    if options.untracked_files == UntrackedMode::No && !options.ignored {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut walk_options = crate::worktree::WorktreeWalkOptions::new();
    walk_options.include_ignored = options.ignored;
    walk_options.recursive = true;
    if !options.pathspec.is_empty() {
        walk_options.prefix(BStr::new(&options.pathspec));
    }
    let entries = worktree.walk(ignore, &walk_options)?;

    // Every path the index knows, plus every directory that contains one. Membership of
    // `tracked_dirs` is what distinguishes a directory whose untracked children are reported
    // individually from one that is itself reported as a single untracked entry.
    let mut tracked: BTreeSet<BString> = BTreeSet::new();
    let mut tracked_dirs: BTreeSet<BString> = BTreeSet::new();
    for entry in index.entries() {
        tracked.insert(entry.path.clone());
        let bytes: &[u8] = entry.path.as_ref();
        for (i, byte) in bytes.iter().enumerate() {
            if *byte == b'/' {
                tracked_dirs.insert(BString::from(&bytes[..i]));
            }
        }
    }

    let mut ignored_paths: BTreeSet<BString> = BTreeSet::new();
    let mut untracked_files: BTreeSet<BString> = BTreeSet::new();
    let mut untracked_dirs: BTreeSet<BString> = BTreeSet::new();

    for entry in entries {
        let path = entry.path;
        let is_dir = entry.kind == crate::ignore::PathKind::Directory;
        if entry.ignored {
            let mut path = path;
            if is_dir {
                path.push(b'/');
            }
            ignored_paths.insert(path);
            continue;
        }
        if options.untracked_files == UntrackedMode::No {
            continue;
        }
        if is_dir {
            // The walk has already declined to descend into an ignored directory, so a
            // directory seen here is untracked unless some tracked path lives under it.
            if !tracked_dirs.contains(&path) {
                untracked_dirs.insert(path);
            }
        } else if !tracked.contains(&path) {
            untracked_files.insert(path);
        }
    }

    // An untracked directory that is itself a repository (it has a `.git`) is reported as one
    // `dir/` entry in every mode and never descended into; a tracked gitlink is not untracked
    // content at all.
    if let Some(root) = worktree.root() {
        use std::os::unix::ffi::OsStrExt;
        let nested: Vec<BString> = untracked_dirs
            .iter()
            .filter(|dir| {
                root.join(std::ffi::OsStr::from_bytes(dir.as_ref()))
                    .join(".git")
                    .exists()
            })
            .cloned()
            .collect();
        for dir in nested {
            let mut prefix = dir.clone();
            prefix.push(b'/');
            untracked_files.retain(|file| !file.starts_with(&prefix[..]));
            untracked_dirs.retain(|d| d != &dir && !d.starts_with(&prefix[..]));
            if !tracked.contains(&dir) {
                untracked_files.insert(prefix);
            }
        }
    }

    // A directory with nothing untracked inside — empty, or holding only ignored files — is not
    // reported: Git does not track directories, so it has nothing to say about them.
    untracked_dirs.retain(|dir| {
        let mut prefix = dir.clone();
        prefix.push(b'/');
        untracked_files
            .iter()
            .any(|file| file.starts_with(&prefix[..]))
    });

    let mut untracked: BTreeSet<BString> = BTreeSet::new();
    match options.untracked_files {
        UntrackedMode::No => {}
        UntrackedMode::All => untracked.extend(untracked_files),
        UntrackedMode::Normal => {
            // `normal` reports the shallowest untracked directory once, instead of every file
            // under it, and nothing is reported twice: a path under a directory that is already
            // an entry of its own is dropped.
            for dir in &untracked_dirs {
                if !has_untracked_ancestor(BStr::new(dir), &untracked_dirs) {
                    let mut path = dir.clone();
                    path.push(b'/');
                    untracked.insert(path);
                }
            }
            for file in untracked_files {
                if !has_untracked_ancestor(BStr::new(&file), &untracked_dirs) {
                    untracked.insert(file);
                }
            }
        }
    }

    // BTreeSet iteration is byte order, which is the order the differential tests compare
    // against; the sets also deduplicate, as two walk routes cannot report one path twice.
    Ok((
        untracked.into_iter().collect(),
        ignored_paths.into_iter().collect(),
    ))
}

/// Whether any ancestor of `path` is a directory in `dirs`.
///
/// The path itself is not its own ancestor, so an untracked directory is not suppressed by
/// being in the set it is being tested against.
fn has_untracked_ancestor(path: &BStr, dirs: &BTreeSet<BString>) -> bool {
    let bytes: &[u8] = path.as_ref();
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'/' {
            let candidate = &bytes[..i];
            if dirs.contains::<BStr>(BStr::new(candidate)) {
                return true;
            }
        }
    }
    false
}
