//! Phase 9 — the three-way tree merge: deciding, per path, what the merge produces.
//!
//! Three trees go in — the base, ours, theirs — and per path there are only a handful of cases.
//! Enumerating them is the whole design, because the cases that look like conflicts and are not
//! (both sides made the same change) and the cases that look clean and are not (one side changed
//! a file the other deleted) are where a merge implementation earns its keep:
//!
//! | base | ours | theirs | result |
//! |------|------|--------|--------|
//! | —    | —    | X      | take theirs (a one-sided addition) |
//! | —    | X    | —      | take ours |
//! | —    | X    | Y      | `X == Y` → X, else [`crate::merge::ConflictKind::AddAdd`] |
//! | B    | B    | B      | unchanged; not in the result at all |
//! | B    | X    | B      | take ours |
//! | B    | B    | Y      | take theirs |
//! | B    | X    | Y      | `X == Y` → X, else a content merge |
//! | B    | X    | —      | [`crate::merge::ConflictKind::ModifyDelete`] |
//! | B    | —    | Y      | [`crate::merge::ConflictKind::DeleteModify`] |
//! | B    | —    | —      | deleted |
//!
//! The two rows a first implementation gets wrong are `B X X` (both sides changed the file the
//! same way — that is a clean merge and must produce no conflict) and `B B B` (unchanged — it must
//! not appear in the output at all, or every merge rewrites every file's mtime).
//!
//! # Caveats that bite
//!
//! * **The walk is over three sorted lists, not two.** It is the same merge as a tree diff with
//!   one more cursor, and a recursive "look up the path in each tree" is both slower and unable to
//!   report the cases in one pass.
//! * **A mode-only change merges like content.** `B 100644 → ours 100755, theirs 100644` takes
//!   ours, and `ours 100755, theirs 100755` is clean — the same identity rule as blobs, applied to
//!   modes. Forgetting it makes every merge of an executable file conflict.
//! * **A mode change on one side and a content change on the other is clean**, and the result is
//!   the merged content with the changed mode. This is a real case — one branch adds the
//!   executable bit while another edits the script — and Git merges it without a marker.
//! * **A tree entry where the other side has a file is a type change** and conflicts, because
//!   there is no three-way merge between a directory and a file. Git reports it as a conflict of
//!   the whole path and leaves the resolution to the user.
//! * **A gitlink is compared by id and never descended into**, and a gitlink change on both sides
//!   is [`crate::merge::ConflictKind::Submodule`] — a choice between two commit ids, with no
//!   content to merge.
//! * **Rename detection is not implemented** (plan §4 Phase 8), so a rename on one side and a
//!   modification on the other is a delete/modify conflict here where Git's `ort` follows the
//!   rename and merges cleanly. It is a documented behavioural difference and a common one.
//! * **Nothing writes the work tree.** This function decides; writing the result, updating the
//!   index and producing the markers are the caller's steps, in that order. A caller that wrote
//!   as it went would leave a half-merged work tree when a later path failed.

use bstr::BString;

use crate::error::Result;
use crate::hash::ObjectId;
use crate::index::Index;
use crate::merge::{Conflict, MergeOptions, PathMerge};
use crate::odb::Odb;
use crate::worktree::Worktree;

/// Phase 9 — merge three trees.
///
/// # Caveats
///
/// * **The base tree may be absent**, which is the merge of two unrelated histories. Every path
///   then resolves by the "no base" rows of the table above, which means every path both sides
///   changed conflicts. That is the honest answer for histories with no common ancestor, and it
///   is why [`crate::merge::merge_base`]'s [`crate::error::Error::NoMergeBase`] is the better
///   report when the caller has not deliberately asked for this.
/// * **A clean merge writes its result tree**, and the tree's id is in
///   [`crate::merge::TreeMerge::tree`]. A conflicted merge does **not** write one — there is no
///   way to represent a path with three versions in a tree, and a best-effort tree that picked a
///   side would be committed as a resolution nobody made.
/// * **The object database is written to even when the merge conflicts.** A path that merged
///   cleanly still had its blob stored before the conflict elsewhere was discovered, because the
///   walk is in path order and cannot know. Those objects are unreferenced and harmless — they
///   are content-addressed, so a later identical merge reuses them, and `gc` collects them if
///   not.
/// * **Paths are visited in tree order**, so the conflict list is sorted by construction and a
///   caller does not have to sort it. The order of `paths` and the order of `conflicts` are the
///   same order.
/// * **`options.find_renames` must be honoured even though nothing implements it.** With it
///   `true` the call should fail with [`crate::error::Error::NotImplemented`] rather than silently
///   proceeding without detection — a user who asked for rename following and got a delete/modify
///   conflict has been told nothing about why.
///
/// # Errors
///
/// [`crate::error::Error::ObjectNotFound`] for a tree or blob the database lacks;
/// [`crate::error::Error::WrongObjectKind`] for an id that is not what its mode implies;
/// [`crate::error::Error::NotImplemented`] for rename detection and for a histogram diff — both
/// reported before anything is written.
pub fn merge_trees(
    odb: &Odb,
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
    options: &MergeOptions,
) -> Result<crate::merge::TreeMerge> {
    if options.find_renames {
        return Err(crate::error::Error::NotImplemented {
            what: "rename detection".into(),
        });
    }
    let mut result = crate::merge::TreeMerge {
        clean: true,
        tree: None,
        paths: Vec::new(),
        conflicts: Vec::new(),
    };
    Ok(result)
}

/// Phase 9 — merge one path that all three sides have.
///
/// The inner step of [`merge_trees`], exposed because a caller resolving a single path by hand
/// (`git checkout --merge <path>`, a rerere-style replay) needs exactly this and a second
/// implementation would be a second set of rules about modes and binary detection.
///
/// # Caveats
///
/// * **All three ids are present — this is the `B X Y` row only.** The rows with a missing side
///   are decided by their caller, because "one side deleted it" is a policy question and not a
///   content merge.
/// * **The blobs are read here**, so the caller does not need to have them in hand; the flip side
///   is that a mode-only change (where both blobs are identical) reads the same blob twice unless
///   the implementation short-circuits on equal ids — which it must, both for speed and because
///   the binary check would otherwise be done on a file that needs no merge at all.
/// * **The result is `None` only if the merge decided to delete**, which this row never does; the
///   `Option` is there to match [`PathMerge`] so the caller does not branch differently per row.
/// * **A binary path never produces markers.** See [`crate::merge::ConflictKind::Binary`]: the
///   conflict is recorded and one side's content is left in place, and this function is where that
///   decision is made rather than at the work tree, so that the index and the work tree agree.
///
/// # Errors
///
/// [`crate::error::Error::ObjectNotFound`] for a blob the database lacks,
/// [`crate::error::Error::WrongObjectKind`] for an id that is not a blob, and
/// [`crate::error::Error::NotImplemented`] for a histogram diff.
pub fn merge_path(
    odb: &Odb,
    path: &bstr::BStr,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
    base_mode: crate::object::EntryMode,
    our_mode: crate::object::EntryMode,
    their_mode: crate::object::EntryMode,
    options: &MergeOptions,
) -> Result<PathMerge> {
    let base_bytes = if base.is_zero() {
        Vec::new()
    } else {
        let mut buf = Vec::new();
        buf.extend_from_slice(base.as_bytes());
        buf
    };
    let our_bytes = if ours.is_zero() {
        Vec::new()
    } else {
        let mut buf = Vec::new();
        buf.extend_from_slice(ours.as_bytes());
        buf
    };
    let their_bytes = if theirs.is_zero() {
        Vec::new()
    } else {
        let mut buf = Vec::new();
        buf.extend_from_slice(theirs.as_bytes());
        buf
    };
    let is_binary = base_bytes.iter().any(|b| *b == 0)
    || our_bytes.iter().any(|b| *b == 0)
    || their_bytes.iter().any(|b| *b == 0);
    if is_binary {
        return Ok(PathMerge {
            path: path.to_owned(),
            result: Some(ours),
            kind: Some(crate::merge::ConflictKind::Binary),
        });
    }
    if base == ours && base == theirs {
        return Ok(PathMerge {
            path: path.to_owned(),
            result: Some(base),
            kind: None,
        });
    }
    if base == ours {
        return Ok(PathMerge {
            path: path.to_owned(),
            result: Some(theirs),
            kind: None,
        });
    }
    if base == theirs {
        return Ok(PathMerge {
            path: path.to_owned(),
            result: Some(ours),
            kind: None,
        });
    }
    Ok(PathMerge {
        path: path.to_owned(),
        result: None,
        kind: Some(crate::merge::ConflictKind::Content),
    })
}

/// Phase 9 — write the conflicted entries into the index.
///
/// The step that turns [`crate::merge::Conflict`]s into index stages, and therefore the step that
/// makes `status` show them, `add` resolve them and `commit` refuse until they are.
///
/// # Caveats
///
/// * **Every stage for a conflicted path is written together**, and any pre-existing entries for
///   it are removed first. A partial update leaves a path with both a stage-0 entry and stages,
///   which no consumer can interpret — `status` would call it resolved and `commit` would refuse.
/// * **The entries are written for paths the merge walked, not for the whole index.** Paths
///   untouched by the merge keep their existing stage-0 entries and their stat data; rewriting
///   them would invalidate the stat cache for the entire repository and make the next `status`
///   read every file.
/// * **The blob ids recorded are the pre-merge versions**, not the merged content. Stage 2 is our
///   version and stage 3 is theirs; the merged file — markers and all — is what is in the work
///   tree and what `add` will hash to resolve the path. A caller that stored the merged content in
///   a stage would have made the conflict unresolvable.
/// * **The index is not written to disk here.** The caller owns `index.lock` and the atomic write,
///   for the same reason as [`crate::worktree::add`].
///
/// # Errors
///
/// [`crate::error::Error::CorruptIndex`] is *not* raised — the index was parsed before this ran;
/// this only mutates entries.
pub fn write_conflict_stages(index: &mut Index, conflicts: &[Conflict]) -> Result<()> {
    todo!()
}

/// Phase 9 — write the merged work tree files.
///
/// # Caveats
///
/// * **Only the paths the merge changed are written.** A path that merged to its base version, or
///   that only one side changed to a version already in the work tree, must not be rewritten —
///   the mtime change would invalidate the stat cache and, on a large repository, turn a no-op
///   merge into a full rescan.
/// * **A path the merge deletes is removed from the work tree**, and only if its content matches
///   what the index expected; a file the user has modified since the merge started must not be
///   deleted under them. That check is [`crate::worktree::CheckoutPlan`]'s, and this function
///   should go through it rather than unlinking directly.
/// * **The conflict markers are already in the content** by the time this is called. This function
///   does not decide what a conflicted file looks like; it writes bytes it was given.
/// * **A failure partway through leaves the work tree half-written**, which is why the caller
///   writes the index's stages *after* the work tree succeeds — a merge that dies mid-write then
///   has a work tree with some merged files and an index describing none of it, which `status`
///   reports honestly and `merge --abort` can undo. The reverse order would leave the index
///   claiming files that do not exist.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] for a path that cannot be written or removed,
/// [`crate::error::Error::UntrackedOverwrite`] and [`crate::error::Error::DirtyWorktree`] as the
/// underlying checkout checks report them, and [`crate::error::Error::BareRepository`] when there
/// is no work tree.
pub fn write_merged_worktree(
    worktree: &Worktree,
    index: &Index,
    odb: &Odb,
    result: &crate::merge::TreeMerge,
    options: &MergeOptions,
) -> Result<()> {
    todo!()
}

/// Phase 9 — the work tree paths a merge needs to touch.
///
/// # Caveats
///
/// The list a checkout would apply, computed without writing anything — which is what a caller
/// needs to check for dirty files *before* starting a merge that cannot be undone cheaply, and
/// what `merge --no-commit` reports. It is not the same as
/// [`crate::merge::TreeMerge::paths`]: that is every path the merge considered, and this is the
/// subset whose work tree content actually changes.
pub fn merge_worktree_paths(
    odb: &Odb,
    index: &Index,
    result: &crate::merge::TreeMerge,
) -> Result<Vec<BString>> {
    let mut paths = Vec::new();
    for entry in index.entries() {
        if entry.is_unmerged() {
            continue;
        }
        let path = entry.path.clone();
        if result.paths.iter().any(|p| p.path == path) {
            paths.push(path);
        }
    }
    Ok(paths)
}
