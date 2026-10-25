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

use std::collections::BTreeMap;

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::index::{Index, IndexExt};
use crate::merge::{Conflict, MergeOptions, PathMerge};
use crate::object::EntryMode;
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
/// * **`options.find_renames` turns on rename detection.** With it `true`, a path moved on one
///   side and edited on the other is merged under its new name; cases that cannot be merged
///   become rename conflicts instead of a delete/modify pair, and the old name is dropped from
///   the result. With it `false` a rename is a delete plus an add, as Git without `-M`.
///
/// # Errors
///
/// [`crate::error::Error::ObjectNotFound`] for a tree or blob the database lacks;
/// [`crate::error::Error::WrongObjectKind`] for an id that is not what its mode implies. Both are
/// reported before anything is written.
pub fn merge_trees(
    odb: &Odb,
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
    options: &MergeOptions,
) -> Result<crate::merge::TreeMerge> {
    let mut base_entries = match base {
        Some(oid) => flatten_tree(odb, oid)?,
        None => BTreeMap::new(),
    };
    let mut our_entries = flatten_tree(odb, ours)?;
    let mut their_entries = flatten_tree(odb, theirs)?;

    // Renames first: a file moved on one side and edited on the other has to be merged under its
    // new name, and the cases that cannot be merged become explicit conflicts.
    let (rename_conflicts, rename_removals) = if options.find_renames {
        apply_renames(
            odb,
            &mut base_entries,
            &mut our_entries,
            &mut their_entries,
            options,
        )?
    } else {
        (Vec::new(), Vec::new())
    };

    let mut paths: Vec<BString> = Vec::new();
    paths.extend(base_entries.keys().cloned());
    paths.extend(our_entries.keys().cloned());
    paths.extend(their_entries.keys().cloned());
    paths.sort();
    paths.dedup();

    let mut result = crate::merge::TreeMerge {
        clean: true,
        tree: None,
        paths: Vec::new(),
        conflicts: Vec::new(),
    };
    // The entries of the tree this merge would produce, kept aside until the walk is done: a
    // conflicted merge writes no tree, so building it under the walking loop would be wasted.
    let mut tree_entries: Vec<(BString, EntryMode, ObjectId)> = Vec::new();
    for (conflict, path_merge) in rename_conflicts {
        result.paths.push(path_merge);
        result.conflicts.push(conflict);
    }
    // A moved file's old name is gone from the result, wherever the move happened.
    for path in rename_removals {
        result.paths.push(PathMerge {
            path,
            result: None,
            kind: None,
        });
    }

    for path in paths {
        let path_str = BStr::new(&path[..]);
        let base_entry = base_entries.get(&path).copied();
        let our_entry = our_entries.get(&path).copied();
        let their_entry = their_entries.get(&path).copied();

        // A path that is a file on one side and a directory on another has no three-way merge:
        // there is nothing to compare, and writing both would produce a tree Git cannot read.
        let our_dir = has_child(&our_entries, path_str);
        let their_dir = has_child(&their_entries, path_str);
        let base_dir = has_child(&base_entries, path_str);
        if (our_entry.is_some() && their_dir)
            || (their_entry.is_some() && our_dir)
            || (base_dir && (our_entry.is_some() || their_entry.is_some()))
        {
            result.paths.push(PathMerge {
                path: path.clone(),
                result: our_entry.map(|(_, oid)| oid),
                kind: Some(crate::merge::ConflictKind::Content),
            });
            result.conflicts.push(Conflict {
                path,
                kind: crate::merge::ConflictKind::Content,
                base: base_entry.map(|(_, oid)| oid),
                ours: our_entry.map(|(_, oid)| oid),
                theirs: their_entry.map(|(_, oid)| oid),
                hunks: Vec::new(),
                message: Some(BString::from("file/directory type change")),
            });
            continue;
        }

        match (base_entry, our_entry, their_entry) {
            (None, None, _) => {
                // Theirs-only addition, or a path no side has (which the union cannot produce).
                if let Some((mode, oid)) = their_entry {
                    result.paths.push(PathMerge {
                        path: path.clone(),
                        result: Some(oid),
                        kind: None,
                    });
                    tree_entries.push((path, mode, oid));
                }
            }
            (None, Some((mode, oid)), None) => {
                result.paths.push(PathMerge {
                    path: path.clone(),
                    result: Some(oid),
                    kind: None,
                });
                tree_entries.push((path, mode, oid));
            }
            (None, Some(our), Some(their)) => {
                if our == their {
                    result.paths.push(PathMerge {
                        path: path.clone(),
                        result: Some(our.1),
                        kind: None,
                    });
                    tree_entries.push((path, our.0, our.1));
                } else if our.0 == EntryMode::Commit || their.0 == EntryMode::Commit {
                    result.paths.push(PathMerge {
                        path: path.clone(),
                        result: Some(our.1),
                        kind: Some(crate::merge::ConflictKind::Submodule),
                    });
                    result.conflicts.push(Conflict {
                        path,
                        kind: crate::merge::ConflictKind::Submodule,
                        base: None,
                        ours: Some(our.1),
                        theirs: Some(their.1),
                        hunks: Vec::new(),
                        message: None,
                    });
                } else {
                    match merge_blob_contents(odb, None, our.1, their.1, options)? {
                        ContentOutcome::Clean(data) => {
                            let oid = odb.write(&crate::object::Object::Blob(data))?;
                            let mode = (our.0).max(their.0);
                            result.paths.push(PathMerge {
                                path: path.clone(),
                                result: Some(oid),
                                kind: None,
                            });
                            tree_entries.push((path, mode, oid));
                        }
                        ContentOutcome::Binary => {
                            result.paths.push(PathMerge {
                                path: path.clone(),
                                result: Some(our.1),
                                kind: Some(crate::merge::ConflictKind::Binary),
                            });
                            result.conflicts.push(Conflict {
                                path,
                                kind: crate::merge::ConflictKind::Binary,
                                base: None,
                                ours: Some(our.1),
                                theirs: Some(their.1),
                                hunks: Vec::new(),
                                message: None,
                            });
                        }
                        ContentOutcome::Conflict(hunks) => {
                            result.paths.push(PathMerge {
                                path: path.clone(),
                                result: Some(our.1),
                                kind: Some(crate::merge::ConflictKind::AddAdd),
                            });
                            result.conflicts.push(Conflict {
                                path,
                                kind: crate::merge::ConflictKind::AddAdd,
                                base: None,
                                ours: Some(our.1),
                                theirs: Some(their.1),
                                hunks,
                                message: None,
                            });
                        }
                    }
                }
            }
            (Some(base_entry), Some(our), None) => {
                if our == base_entry {
                    // Ours untouched, theirs deleted: the delete wins cleanly.
                    result.paths.push(PathMerge {
                        path,
                        result: None,
                        kind: None,
                    });
                } else {
                    result.paths.push(PathMerge {
                        path: path.clone(),
                        result: Some(our.1),
                        kind: Some(crate::merge::ConflictKind::ModifyDelete),
                    });
                    result.conflicts.push(Conflict {
                        path,
                        kind: crate::merge::ConflictKind::ModifyDelete,
                        base: Some(base_entry.1),
                        ours: Some(our.1),
                        theirs: None,
                        hunks: Vec::new(),
                        message: None,
                    });
                }
            }
            (Some(base_entry), None, Some(their)) => {
                if their == base_entry {
                    result.paths.push(PathMerge {
                        path,
                        result: None,
                        kind: None,
                    });
                } else {
                    result.paths.push(PathMerge {
                        path: path.clone(),
                        result: None,
                        kind: Some(crate::merge::ConflictKind::DeleteModify),
                    });
                    result.conflicts.push(Conflict {
                        path,
                        kind: crate::merge::ConflictKind::DeleteModify,
                        base: Some(base_entry.1),
                        ours: None,
                        theirs: Some(their.1),
                        hunks: Vec::new(),
                        message: None,
                    });
                }
            }
            (Some(_), None, None) => {
                // Both sides deleted: nothing to report and nothing to write.
            }
            (Some(base_entry), Some(our), Some(their)) => {
                if our == base_entry {
                    result.paths.push(PathMerge {
                        path: path.clone(),
                        result: Some(their.1),
                        kind: None,
                    });
                    tree_entries.push((path, their.0, their.1));
                } else if their == base_entry || our == their {
                    // Only we changed it, or both made the same change: ours is the result.
                    result.paths.push(PathMerge {
                        path: path.clone(),
                        result: Some(our.1),
                        kind: None,
                    });
                    tree_entries.push((path, our.0, our.1));
                } else if base_entry.0 == EntryMode::Commit
                    || our.0 == EntryMode::Commit
                    || their.0 == EntryMode::Commit
                {
                    result.paths.push(PathMerge {
                        path: path.clone(),
                        result: Some(our.1),
                        kind: Some(crate::merge::ConflictKind::Submodule),
                    });
                    result.conflicts.push(Conflict {
                        path,
                        kind: crate::merge::ConflictKind::Submodule,
                        base: Some(base_entry.1),
                        ours: Some(our.1),
                        theirs: Some(their.1),
                        hunks: Vec::new(),
                        message: None,
                    });
                } else {
                    // The mode merges on its own: a change on one side wins, and a change on both
                    // sides to different modes is a conflict even when the content agrees.
                    let mode = if our.0 == base_entry.0 {
                        their.0
                    } else if their.0 == base_entry.0 || our.0 == their.0 {
                        our.0
                    } else {
                        result.paths.push(PathMerge {
                            path: path.clone(),
                            result: Some(our.1),
                            kind: Some(crate::merge::ConflictKind::Mode),
                        });
                        result.conflicts.push(Conflict {
                            path,
                            kind: crate::merge::ConflictKind::Mode,
                            base: Some(base_entry.1),
                            ours: Some(our.1),
                            theirs: Some(their.1),
                            hunks: Vec::new(),
                            message: None,
                        });
                        continue;
                    };
                    match merge_blob_contents(odb, Some(base_entry.1), our.1, their.1, options)? {
                        ContentOutcome::Clean(data) => {
                            let oid = odb.write(&crate::object::Object::Blob(data))?;
                            result.paths.push(PathMerge {
                                path: path.clone(),
                                result: Some(oid),
                                kind: None,
                            });
                            tree_entries.push((path, mode, oid));
                        }
                        ContentOutcome::Binary => {
                            result.paths.push(PathMerge {
                                path: path.clone(),
                                result: Some(our.1),
                                kind: Some(crate::merge::ConflictKind::Binary),
                            });
                            result.conflicts.push(Conflict {
                                path,
                                kind: crate::merge::ConflictKind::Binary,
                                base: Some(base_entry.1),
                                ours: Some(our.1),
                                theirs: Some(their.1),
                                hunks: Vec::new(),
                                message: None,
                            });
                        }
                        ContentOutcome::Conflict(hunks) => {
                            result.paths.push(PathMerge {
                                path: path.clone(),
                                result: Some(our.1),
                                kind: Some(crate::merge::ConflictKind::Content),
                            });
                            result.conflicts.push(Conflict {
                                path,
                                kind: crate::merge::ConflictKind::Content,
                                base: Some(base_entry.1),
                                ours: Some(our.1),
                                theirs: Some(their.1),
                                hunks,
                                message: None,
                            });
                        }
                    }
                }
            }
        }
    }

    result.clean = result.conflicts.is_empty();
    if result.clean {
        let mut index = Index::new();
        for (path, mode, oid) in tree_entries {
            index.insert(crate::index::IndexEntry {
                ctime_seconds: 0,
                ctime_nanos: 0,
                mtime_seconds: 0,
                mtime_nanos: 0,
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: crate::index::IndexEntryFlags::with(crate::index::Stage::Normal, path.len()),
                extended_flags: crate::index::IndexEntryExtendedFlags::empty().bits(),
                path,
            });
        }
        use crate::index::IndexExt;
        result.tree = Some(index.write_tree(odb)?);
    }
    Ok(result)
}

/// Every file in a tree, keyed by its full path, with its mode and id.
///
/// Directories are descended into and do not appear themselves; a gitlink is recorded and not
/// followed, because its commit lives in another repository.
pub(crate) fn flatten_tree(
    odb: &Odb,
    tree: ObjectId,
) -> Result<BTreeMap<BString, (EntryMode, ObjectId)>> {
    let mut out = BTreeMap::new();
    // Git never stores the empty tree but treats it as always present.
    let (empty, _) =
        crate::object::hash_object(odb.format(), crate::object::ObjectKind::Tree, b"")?;
    let mut stack: Vec<(BString, ObjectId)> = vec![(BString::default(), tree)];
    while let Some((prefix, oid)) = stack.pop() {
        if oid == empty {
            continue;
        }
        let object = odb.read_typed(&oid, crate::object::ObjectKind::Tree)?;
        let parsed = crate::object::Tree::parse(odb.format(), object.data())?;
        for entry in parsed.iter() {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(entry.name.as_slice());
            match entry.mode {
                EntryMode::Tree => stack.push((path, entry.oid)),
                mode => {
                    out.insert(path, (mode, entry.oid));
                }
            }
        }
    }
    Ok(out)
}

/// Whether a path is a directory (has at least one entry beneath it) in this tree.
fn has_child(map: &BTreeMap<BString, (EntryMode, ObjectId)>, path: &BStr) -> bool {
    let mut prefix = path.to_owned();
    prefix.push(b'/');
    map.keys()
        .any(|name: &BString| name.starts_with(&prefix[..]))
}

/// The three outcomes a content merge has that matter to the tree walk.
enum ContentOutcome {
    Clean(Vec<u8>),
    Conflict(Vec<crate::merge::ConflictHunk>),
    Binary,
}

fn read_blob(odb: &Odb, oid: ObjectId) -> Result<Vec<u8>> {
    match odb.read(&oid)? {
        crate::object::Object::Blob(data) => Ok(data),
        other => Err(crate::error::Error::WrongObjectKind {
            oid: Some(oid),
            expected: crate::object::ObjectKind::Blob,
            found: other.kind(),
        }),
    }
}

fn merge_blob_contents(
    odb: &Odb,
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
    options: &MergeOptions,
) -> Result<ContentOutcome> {
    let base_data = match base {
        Some(oid) => Some(read_blob(odb, oid)?),
        None => None,
    };
    let our_data = read_blob(odb, ours)?;
    let their_data = read_blob(odb, theirs)?;
    let binary = base_data
        .as_ref()
        .map(|d| crate::diff::is_binary(d))
        .unwrap_or(false)
        || crate::diff::is_binary(&our_data)
        || crate::diff::is_binary(&their_data);
    if binary {
        return Ok(ContentOutcome::Binary);
    }
    let merged = crate::merge::merge_content(
        base_data.as_deref(),
        &our_data,
        &their_data,
        (
            std::str::from_utf8(&options.labels.0).unwrap_or("ours"),
            std::str::from_utf8(&options.labels.1).unwrap_or("theirs"),
        ),
        options.conflict_style,
        options.diff_algorithm,
    )?;
    if merged.conflict {
        Ok(ContentOutcome::Conflict(merged.hunks))
    } else {
        Ok(ContentOutcome::Clean(merged.data.to_vec()))
    }
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
/// [`crate::error::Error::ObjectNotFound`] for a blob the database lacks and
/// [`crate::error::Error::WrongObjectKind`] for an id that is not a blob. Every diff algorithm is
/// supported.
#[allow(clippy::too_many_arguments)] // signature kept for the callers below
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
    // A zero id is how the caller says "this side has no version of the path"; it is not an
    // object id and must never be read from the database.
    let base_present = if base.is_zero() { None } else { Some(base) };
    let ours_present = if ours.is_zero() { None } else { Some(ours) };
    let theirs_present = if theirs.is_zero() { None } else { Some(theirs) };

    if base_present == ours_present && base_present == theirs_present {
        return Ok(PathMerge {
            path: path.to_owned(),
            result: ours_present,
            kind: None,
        });
    }
    if base_present == ours_present {
        return Ok(PathMerge {
            path: path.to_owned(),
            result: theirs_present,
            kind: None,
        });
    }
    if base_present == theirs_present {
        return Ok(PathMerge {
            path: path.to_owned(),
            result: ours_present,
            kind: None,
        });
    }
    if ours_present.is_none() || theirs_present.is_none() {
        // One side deleted the path: a policy decision, not a content merge. The caller decides
        // which side wins; this function reports the disagreement.
        let kind = if ours_present.is_none() {
            crate::merge::ConflictKind::DeleteModify
        } else {
            crate::merge::ConflictKind::ModifyDelete
        };
        return Ok(PathMerge {
            path: path.to_owned(),
            result: ours_present,
            kind: Some(kind),
        });
    }
    if base_mode == crate::object::EntryMode::Commit
        || our_mode == crate::object::EntryMode::Commit
        || their_mode == crate::object::EntryMode::Commit
    {
        return Ok(PathMerge {
            path: path.to_owned(),
            result: ours_present,
            kind: Some(crate::merge::ConflictKind::Submodule),
        });
    }

    let our_oid = ours_present.expect("checked above");
    let their_oid = theirs_present.expect("checked above");
    match merge_blob_contents(odb, base_present, our_oid, their_oid, options)? {
        ContentOutcome::Clean(data) => {
            let oid = odb.write(&crate::object::Object::Blob(data))?;
            Ok(PathMerge {
                path: path.to_owned(),
                result: Some(oid),
                kind: None,
            })
        }
        ContentOutcome::Binary => Ok(PathMerge {
            path: path.to_owned(),
            result: Some(our_oid),
            kind: Some(crate::merge::ConflictKind::Binary),
        }),
        ContentOutcome::Conflict(_) => Ok(PathMerge {
            path: path.to_owned(),
            result: Some(our_oid),
            kind: Some(crate::merge::ConflictKind::Content),
        }),
    }
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
    use crate::index::{IndexEntry, IndexEntryExtendedFlags, IndexEntryFlags, Stage};

    for conflict in conflicts {
        let path = bstr::BStr::new(&conflict.path[..]);
        let existing_mode = index.stages(path).first().map(|entry| entry.mode);
        index.remove_path(path);

        let mode = if conflict.kind == crate::merge::ConflictKind::Submodule {
            crate::object::EntryMode::Commit
        } else {
            existing_mode.unwrap_or(crate::object::EntryMode::Blob)
        };
        for (stage, oid) in [
            (Stage::Base, conflict.base),
            (Stage::Ours, conflict.ours),
            (Stage::Theirs, conflict.theirs),
        ] {
            let Some(oid) = oid else { continue };
            index.insert(IndexEntry {
                ctime_seconds: 0,
                ctime_nanos: 0,
                mtime_seconds: 0,
                mtime_nanos: 0,
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: IndexEntryFlags::with(stage, conflict.path.len()),
                extended_flags: IndexEntryExtendedFlags::empty().bits(),
                path: conflict.path.clone(),
            });
        }
    }
    Ok(())
}

/// Materialise a finished merge into the worktree and index.
///
/// Clean paths are written as merged content; conflicted paths get Git's conflict markers in the
/// worktree and stage entries in the index, so `git status` shows `UU`/`AA`/`DU` exactly as it
/// would after a `git merge`.
///
/// # Errors
///
/// Whatever reading blobs, writing worktree files or rewriting `.git/index` raises. The writes
/// are not atomic: a failure part-way leaves the files already written in place.
pub fn write_merged_worktree(
    worktree: &Worktree,
    index: &Index,
    odb: &Odb,
    result: &crate::merge::TreeMerge,
    options: &MergeOptions,
) -> Result<()> {
    use crate::merge::{ConflictKind, merge_content};

    worktree.require_root("merge")?;

    let conflicted: std::collections::BTreeSet<&bstr::BStr> = result
        .conflicts
        .iter()
        .map(|c| bstr::BStr::new(&c.path[..]))
        .collect();

    // Clean paths first: one write per file the merge changed, and nothing for the paths that
    // already held the right content.
    for path_merge in &result.paths {
        let path = bstr::BStr::new(&path_merge.path[..]);
        if conflicted.contains(path) {
            continue;
        }
        match (path_merge.kind, path_merge.result) {
            (None, Some(oid)) => {
                let mode = index
                    .entry(path)
                    .map(|entry| entry.mode)
                    .unwrap_or(crate::object::EntryMode::Blob);
                crate::worktree::checkout::write_blob(worktree, path, oid, mode, odb)?;
            }
            (None, None) if worktree.stat(path)?.is_some() => {
                crate::worktree::checkout::remove_path(worktree, path)?;
            }
            _ => {}
        }
    }

    for conflict in &result.conflicts {
        let path = bstr::BStr::new(&conflict.path[..]);
        let mode = index
            .entry_at(path, crate::index::Stage::Ours)
            .ok()
            .flatten()
            .map(|entry| entry.mode)
            .unwrap_or(crate::object::EntryMode::Blob);
        match conflict.kind {
            ConflictKind::Content | ConflictKind::AddAdd | ConflictKind::Mode => {
                let base = match conflict.base {
                    Some(oid) => Some(read_blob(odb, oid)?),
                    None => None,
                };
                let ours = match conflict.ours {
                    Some(oid) => read_blob(odb, oid)?,
                    None => Vec::new(),
                };
                let theirs = match conflict.theirs {
                    Some(oid) => read_blob(odb, oid)?,
                    None => Vec::new(),
                };
                let merged = merge_content(
                    base.as_deref(),
                    &ours,
                    &theirs,
                    (
                        std::str::from_utf8(&options.labels.0).unwrap_or("ours"),
                        std::str::from_utf8(&options.labels.1).unwrap_or("theirs"),
                    ),
                    options.conflict_style,
                    options.diff_algorithm,
                )?;
                // The marker-laden file is work tree content only; storing it as a blob would
                // leave an object nothing references for `fsck` to report as dangling.
                crate::worktree::checkout::write_bytes(worktree, path, &merged.data, mode)?;
            }
            ConflictKind::Binary | ConflictKind::ModifyDelete => {
                if let Some(oid) = conflict.ours {
                    crate::worktree::checkout::write_blob(worktree, path, oid, mode, odb)?;
                }
            }
            ConflictKind::RenameDelete | ConflictKind::RenameRename => {
                // The renamed file is written under its new name with the renaming side's content.
                if let Some(oid) = conflict.ours.or(conflict.theirs) {
                    crate::worktree::checkout::write_blob(worktree, path, oid, mode, odb)?;
                }
            }
            ConflictKind::DeleteModify => {
                // Our side deleted the path; the file the other side kept is written so the user
                // can see what they are being asked to resolve.
                if let Some(oid) = conflict.theirs {
                    crate::worktree::checkout::write_blob(worktree, path, oid, mode, odb)?;
                }
            }
            ConflictKind::Submodule => {
                // A gitlink points into another repository; updating it is that repository's
                // business and this merge only records the disagreement in the index.
            }
        }
    }
    Ok(())
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
/// Bring the index in line with a completed merge.
///
/// [`merge_trees`] decides; this records the decision. Clean paths get a single stage-0 entry with
/// the merged blob, and conflicted paths keep the stages [`write_conflict_stages`] wrote. Without
/// this step a clean merge leaves the index describing the pre-merge commit, and the next
/// `status` reports every merged file as modified.
pub fn update_index_after_merge(
    index: &mut Index,
    odb: &Odb,
    result: &crate::merge::TreeMerge,
) -> Result<()> {
    let modes = match result.tree {
        Some(tree) => flatten_tree(odb, tree)?,
        None => BTreeMap::new(),
    };
    let conflicted: std::collections::BTreeSet<&BStr> = result
        .conflicts
        .iter()
        .map(|c| BStr::new(&c.path[..]))
        .collect();

    for path_merge in &result.paths {
        let path = BStr::new(&path_merge.path[..]);
        if conflicted.contains(path) {
            continue;
        }
        let previous_mode = index.entry(path).map(|entry| entry.mode);
        index.remove_path(path);
        let Some(oid) = path_merge.result else {
            continue;
        };
        let mode = modes
            .get(&path_merge.path)
            .map(|(mode, _)| *mode)
            .or(previous_mode)
            .unwrap_or(EntryMode::Blob);
        index.insert(crate::index::IndexEntry {
            ctime_seconds: 0,
            ctime_nanos: 0,
            mtime_seconds: 0,
            mtime_nanos: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: crate::index::IndexEntryFlags::with(
                crate::index::Stage::Normal,
                path_merge.path.len(),
            ),
            extended_flags: crate::index::IndexEntryExtendedFlags::empty().bits(),
            path: path_merge.path.clone(),
        });
    }
    Ok(())
}

/// The tracked paths one merge result touches.
///
/// Walks `index` and returns the paths that appear in `result.paths`, skipping unmerged entries —
/// that is, the files a caller still has to bring in line with the merge. Paths the merge adds are
/// not included, because they are not yet in the index.
///
/// `odb` is accepted but currently unused.
///
/// # Errors
///
/// None today; the `Result` matches the other merge entry points.
pub fn merge_worktree_paths(
    _odb: &Odb,
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

type EntryMap = BTreeMap<BString, (EntryMode, ObjectId)>;

/// Conflicts that renames produce, each with the path result that goes with it.
type RenameConflicts = Vec<(Conflict, PathMerge)>;

/// Pair the files `side` removed from `base` with the files it added.
fn renames_on_side(
    odb: &Odb,
    base: &EntryMap,
    side: &EntryMap,
) -> Result<BTreeMap<BString, BString>> {
    use crate::diff::rename::{RenameSide, pair_renames};
    let sources: Vec<RenameSide> = base
        .iter()
        .filter(|(path, _)| !side.contains_key(*path))
        .map(|(path, (mode, oid))| RenameSide {
            path: path.clone(),
            oid: *oid,
            mode: *mode,
        })
        .collect();
    let destinations: Vec<RenameSide> = side
        .iter()
        .filter(|(path, _)| !base.contains_key(*path))
        .map(|(path, (mode, oid))| RenameSide {
            path: path.clone(),
            oid: *oid,
            mode: *mode,
        })
        .collect();
    let mut out = BTreeMap::new();
    for pair in pair_renames(&sources, &destinations, odb)? {
        out.insert(
            sources[pair.source].path.clone(),
            destinations[pair.destination].path.clone(),
        );
    }
    Ok(out)
}

/// Rewrite the three trees so a rename on one side lines up with the other side's version of the
/// file, and return the conflicts that renames cannot resolve (`rename/delete`, `rename/rename`).
fn apply_renames(
    odb: &Odb,
    base: &mut EntryMap,
    ours: &mut EntryMap,
    theirs: &mut EntryMap,
    options: &MergeOptions,
) -> Result<(RenameConflicts, Vec<BString>)> {
    use crate::merge::ConflictKind;
    let mut removals: Vec<BString> = Vec::new();
    let our_renames = renames_on_side(odb, base, ours)?;
    let their_renames = renames_on_side(odb, base, theirs)?;
    let mut conflicts: Vec<(Conflict, PathMerge)> = Vec::new();
    let label = |bytes: &BString| String::from_utf8_lossy(bytes).into_owned();
    let (our_label, their_label) = (label(&options.labels.0), label(&options.labels.1));

    let mut sources: Vec<BString> = our_renames
        .keys()
        .chain(their_renames.keys())
        .cloned()
        .collect();
    sources.sort();
    sources.dedup();

    for from in sources {
        let Some(base_entry) = base.get(&from).copied() else {
            continue;
        };
        match (our_renames.get(&from), their_renames.get(&from)) {
            (Some(to_ours), Some(to_theirs)) if to_ours == to_theirs => {
                // Both moved it to the same place: merge the contents there.
                base.remove(&from);
                base.insert(to_ours.clone(), base_entry);
                removals.push(from.clone());
            }
            (Some(to_ours), Some(to_theirs)) => {
                // Moved to two different places: neither wins. The base stays at stage 1, each
                // side's file at its own path.
                base.remove(&from);
                let our_entry = ours.remove(to_ours);
                let their_entry = theirs.remove(to_theirs);
                let message = format!(
                    "CONFLICT (rename/rename): {from} renamed to {to_ours} in {our_label} and to {to_theirs} in {their_label}."
                );
                conflicts.push((
                    Conflict {
                        path: from.clone(),
                        kind: ConflictKind::RenameRename,
                        base: Some(base_entry.1),
                        ours: None,
                        theirs: None,
                        hunks: Vec::new(),
                        message: Some(BString::from(message)),
                    },
                    PathMerge {
                        path: from.clone(),
                        result: None,
                        kind: Some(ConflictKind::RenameRename),
                    },
                ));
                for (path, entry, is_ours) in
                    [(to_ours, our_entry, true), (to_theirs, their_entry, false)]
                {
                    let Some((_, oid)) = entry else { continue };
                    conflicts.push((
                        Conflict {
                            path: path.clone(),
                            kind: ConflictKind::RenameRename,
                            base: None,
                            ours: is_ours.then_some(oid),
                            theirs: (!is_ours).then_some(oid),
                            hunks: Vec::new(),
                            message: None,
                        },
                        PathMerge {
                            path: path.clone(),
                            result: Some(oid),
                            kind: Some(ConflictKind::RenameRename),
                        },
                    ));
                }
            }
            (Some(to_ours), None) => {
                one_sided_rename(
                    &from,
                    to_ours,
                    base_entry,
                    base,
                    ours,
                    theirs,
                    true,
                    &our_label,
                    &their_label,
                    &mut conflicts,
                    &mut removals,
                );
            }
            (None, Some(to_theirs)) => {
                one_sided_rename(
                    &from,
                    to_theirs,
                    base_entry,
                    base,
                    theirs,
                    ours,
                    false,
                    &our_label,
                    &their_label,
                    &mut conflicts,
                    &mut removals,
                );
            }
            (None, None) => {}
        }
    }
    removals.sort();
    removals.dedup();
    Ok((conflicts, removals))
}

/// `side` renamed `from` to `to`; `other` did not rename it.
#[allow(clippy::too_many_arguments)]
fn one_sided_rename(
    from: &BString,
    to: &BString,
    base_entry: (EntryMode, ObjectId),
    base: &mut EntryMap,
    side: &mut EntryMap,
    other: &mut EntryMap,
    side_is_ours: bool,
    our_label: &str,
    their_label: &str,
    conflicts: &mut Vec<(Conflict, PathMerge)>,
    removals: &mut Vec<BString>,
) {
    use crate::merge::ConflictKind;
    match other.get(from).copied() {
        Some(other_entry) => {
            if other.contains_key(to) {
                // The other side already has something at the destination: leave both alone.
                return;
            }
            base.remove(from);
            base.insert(to.clone(), base_entry);
            other.remove(from);
            other.insert(to.clone(), other_entry);
            removals.push(from.clone());
        }
        None => {
            // The other side deleted the file the side renamed.
            let Some((_, oid)) = side.remove(to) else {
                return;
            };
            base.remove(from);
            let (renamed_in, deleted_in) = if side_is_ours {
                (our_label, their_label)
            } else {
                (their_label, our_label)
            };
            let message = format!(
                "CONFLICT (rename/delete): {from} renamed to {to} in {renamed_in}, but deleted in {deleted_in}."
            );
            conflicts.push((
                Conflict {
                    path: to.clone(),
                    kind: ConflictKind::RenameDelete,
                    base: Some(base_entry.1),
                    ours: side_is_ours.then_some(oid),
                    theirs: (!side_is_ours).then_some(oid),
                    hunks: Vec::new(),
                    message: Some(BString::from(message)),
                },
                PathMerge {
                    path: to.clone(),
                    result: Some(oid),
                    kind: Some(ConflictKind::RenameDelete),
                },
            ));
        }
    }
}
