//! Repository verification: the object-database walk behind `rivet fsck`.
//!
//! Every stored object is read, re-hashed and parsed; the graph is then walked from the refs,
//! their reflogs and the index to find missing objects and to separate the reachable objects from
//! the unreachable ones. An unreachable object that no other object points at is *dangling*,
//! which is what Git reports by default.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::object::{AnyObject, EntryMode, ObjectKind};
use crate::refs::{RefName, RefTarget};
use crate::repo::Repository;

/// Bit set in [`FsckReport::status`] when an object is corrupt.
pub const ERROR_OBJECT: u8 = 1;
/// Bit set when an object reachable from a root is missing.
pub const ERROR_REACHABLE: u8 = 2;
/// Bit set when a ref is broken.
pub const ERROR_REFS: u8 = 8;

/// What to check.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsckOptions {
    /// Apply the stricter tree checks.
    pub strict: bool,
    /// Skip re-hashing and parsing; check only that everything reachable exists.
    pub connectivity_only: bool,
    /// Do not treat reflog entries as roots.
    pub no_reflogs: bool,
}

/// The outcome of a check.
#[derive(Clone, Debug, Default)]
pub struct FsckReport {
    /// Objects examined.
    pub objects: usize,
    /// Diagnostics for corrupt objects and refs, one line each, for stderr.
    pub problems: Vec<String>,
    /// Objects reachable from a root that the database lacks, with the kind they should have.
    pub missing: Vec<(ObjectKind, ObjectId)>,
    /// Unreachable objects nothing points at.
    pub dangling: Vec<(ObjectKind, ObjectId)>,
    /// Every unreachable object.
    pub unreachable: Vec<(ObjectKind, ObjectId)>,
    /// Commits with no parents that are reachable.
    pub roots: Vec<ObjectId>,
    /// Informational lines for stderr.
    pub notices: Vec<String>,
    /// Git's exit status bits; zero means the repository is intact.
    pub status: u8,
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

/// Git's tree-ordering key: a directory sorts as if its name ended in `/`.
fn sort_key(name: &[u8], mode: EntryMode) -> Vec<u8> {
    let mut key = name.to_vec();
    if mode == EntryMode::Tree {
        key.push(b'/');
    }
    key
}

/// The `fsck` messages for one tree's entries, in Git's wording.
fn tree_problems(tree: &crate::object::Tree<'_>) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut duplicate = false;
    let mut unsorted = false;
    let mut previous: Option<Vec<u8>> = None;
    for entry in &tree.entries {
        if !seen.insert(entry.name.to_vec()) {
            duplicate = true;
        }
        let key = sort_key(&entry.name, entry.mode);
        if let Some(prev) = &previous
            && *prev > key
        {
            unsorted = true;
        }
        previous = Some(key);
        if entry.name.as_slice() == b"." {
            out.push("hasDot: contains '.'");
        } else if entry.name.as_slice() == b".." {
            out.push("hasDotdot: contains '..'");
        } else if entry.name.eq_ignore_ascii_case(b".git") {
            out.push("hasDotgit: contains '.git'");
        } else if entry.name.is_empty() {
            out.push("emptyName: contains empty pathname");
        }
    }
    if duplicate {
        out.push("duplicateEntries: contains duplicate file entries");
    }
    if unsorted && !duplicate {
        out.push("treeNotSorted: not properly sorted");
    }
    out.dedup();
    out
}

/// Check the repository.
pub fn fsck(repo: &Repository, options: &FsckOptions) -> Result<FsckReport> {
    let odb = repo.odb();
    let format = repo.format();
    let mut report = FsckReport::default();

    // Pass 1: read every object, record its kind and its outgoing edges.
    let mut kinds: HashMap<ObjectId, ObjectKind> = HashMap::new();
    let mut edges: HashMap<ObjectId, Vec<(ObjectKind, ObjectId)>> = HashMap::new();
    let mut ids = odb.oids_with_alternates()?;
    ids.sort();
    for oid in ids {
        report.objects += 1;
        let object = match odb.read(&oid) {
            Ok(object) => object,
            Err(error) => {
                report
                    .problems
                    .push(format!("error: {oid}: object corrupt or missing: {error}"));
                report.status |= ERROR_OBJECT;
                continue;
            }
        };
        kinds.insert(oid, object.kind());
        if !options.connectivity_only {
            match object.id(format) {
                Ok(actual) if actual == oid => {}
                _ => {
                    report.problems.push(format!("error: hash mismatch {oid}"));
                    report.status |= ERROR_OBJECT;
                    continue;
                }
            }
        }
        let mut out = Vec::new();
        match object.parse() {
            Ok(AnyObject::Commit(commit)) => {
                out.push((ObjectKind::Tree, commit.tree));
                if !odb.is_shallow(&oid) {
                    out.extend(commit.parents.iter().map(|p| (ObjectKind::Commit, *p)));
                }
            }
            Ok(AnyObject::Tree(tree)) => {
                if !options.connectivity_only {
                    for problem in tree_problems(&tree) {
                        report
                            .problems
                            .push(format!("error in tree {oid}: {problem}"));
                        report.status |= ERROR_OBJECT;
                    }
                }
                for entry in &tree.entries {
                    match entry.mode {
                        EntryMode::Commit => {}
                        EntryMode::Tree => out.push((ObjectKind::Tree, entry.oid)),
                        _ => out.push((ObjectKind::Blob, entry.oid)),
                    }
                }
            }
            Ok(AnyObject::Tag(tag)) => out.push((tag.kind, tag.object)),
            Ok(AnyObject::Blob(_)) => {}
            Err(error) => {
                report.problems.push(format!(
                    "error in {} {oid}: {error}",
                    kind_name(object.kind())
                ));
                report.status |= ERROR_OBJECT;
            }
        }
        edges.insert(oid, out);
    }

    // Roots: refs, HEAD, reflogs and the index.
    let mut roots: Vec<(ObjectId, ObjectKind)> = Vec::new();
    let mut ref_names: Vec<RefName> = repo
        .refs()
        .list(b"refs/")?
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    ref_names.push(RefName::head());
    for name in &ref_names {
        match repo.refs().try_read(name) {
            Ok(Some(target)) => {
                let oid = match target {
                    RefTarget::Direct(oid) => Some(oid),
                    RefTarget::Symbolic(_) => repo.refs().resolve_oid(name).ok(),
                };
                match oid {
                    Some(oid) if kinds.contains_key(&oid) => roots.push((oid, kinds[&oid])),
                    Some(oid) => {
                        report
                            .problems
                            .push(format!("error: {name}: invalid sha1 pointer {oid}"));
                        report.status |= ERROR_REFS;
                    }
                    None if name.is_head() => {
                        if let Ok(Some(RefTarget::Symbolic(branch))) = repo.refs().try_read(name) {
                            report.notices.push(format!(
                                "notice: HEAD points to an unborn branch ({})",
                                branch.short_name()
                            ));
                        }
                    }
                    None => {}
                }
            }
            Ok(None) => {}
            Err(error) => {
                report.problems.push(format!("error: {name}: {error}"));
                report.status |= ERROR_REFS;
            }
        }
        if !options.no_reflogs
            && let Ok(entries) = repo.refs().reflog(name)
        {
            for entry in entries {
                for oid in [entry.old, entry.new] {
                    if !oid.is_zero()
                        && let Some(kind) = kinds.get(&oid)
                    {
                        roots.push((oid, *kind));
                    }
                }
            }
        }
    }
    for entry in repo.index().entries() {
        if entry.mode == EntryMode::Commit {
            continue;
        }
        match kinds.get(&entry.oid) {
            Some(kind) => roots.push((entry.oid, *kind)),
            None => {
                report.missing.push((ObjectKind::Blob, entry.oid));
                report.status |= ERROR_REACHABLE;
            }
        }
    }

    // The other work trees keep their detached `HEAD`, their index and their `HEAD` log alive.
    for oid in crate::gc::other_worktree_roots(repo, !options.no_reflogs)? {
        if let Some(kind) = kinds.get(&oid) {
            roots.push((oid, *kind));
        }
    }

    // Pass 2: reachability.
    let mut reachable: HashSet<ObjectId> = HashSet::new();
    let mut missing: BTreeSet<(ObjectId, u8)> = BTreeSet::new();
    let mut stack: Vec<ObjectId> = roots.iter().map(|(oid, _)| *oid).collect();
    while let Some(oid) = stack.pop() {
        if !reachable.insert(oid) {
            continue;
        }
        for (kind, target) in edges.get(&oid).into_iter().flatten() {
            if kinds.contains_key(target) {
                stack.push(*target);
            } else {
                missing.insert((*target, *kind as u8));
            }
        }
    }
    for (oid, kind) in missing {
        let kind = [
            ObjectKind::Blob,
            ObjectKind::Tree,
            ObjectKind::Commit,
            ObjectKind::Tag,
        ]
        .into_iter()
        .find(|k| *k as u8 == kind)
        .unwrap_or(ObjectKind::Blob);
        report.missing.push((kind, oid));
        report.status |= ERROR_REACHABLE;
    }
    report.missing.sort_by_key(|(_, oid)| *oid);
    report.missing.dedup();

    // Root commits: Git reports every commit without parents it checks, reachable or not.
    let mut commits: Vec<ObjectId> = kinds
        .iter()
        .filter(|(_, kind)| **kind == ObjectKind::Commit)
        .map(|(oid, _)| *oid)
        .collect();
    commits.sort();
    for oid in commits {
        let is_root = edges
            .get(&oid)
            .is_some_and(|e| !e.iter().any(|(k, _)| *k == ObjectKind::Commit));
        if is_root {
            report.roots.push(oid);
        }
    }

    // Pass 3: dangling = unreachable and not pointed at by any object.
    let referenced: HashSet<ObjectId> = edges.values().flatten().map(|(_, t)| *t).collect();
    let mut all: Vec<ObjectId> = kinds.keys().copied().collect();
    all.sort();
    for oid in git_table_order(&all) {
        if reachable.contains(&oid) {
            continue;
        }
        let entry = (kinds[&oid], oid);
        report.unreachable.push(entry);
        if !referenced.contains(&oid) {
            report.dangling.push(entry);
        }
    }
    Ok(report)
}

/// `ids` in the order Git's in-memory object table hands them to `fsck`: slot order of an open
/// addressing table indexed by the first four id bytes (little-endian) modulo its size, which
/// starts at 32 and doubles whenever it would pass half full. Ids that collide land in the order
/// given, so `ids` should be sorted for a stable answer.
fn git_table_order(ids: &[ObjectId]) -> Vec<ObjectId> {
    fn slot(id: &ObjectId, size: usize) -> usize {
        let b = id.as_bytes();
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize % size
    }
    fn insert(table: &mut [Option<ObjectId>], id: ObjectId) {
        let mut j = slot(&id, table.len());
        while table[j].is_some() {
            j = (j + 1) % table.len();
        }
        table[j] = Some(id);
    }
    let mut table: Vec<Option<ObjectId>> = Vec::new();
    for (count, id) in ids.iter().enumerate() {
        if table.len() <= count * 2 + 1 {
            let mut grown = vec![None; (table.len() * 2).max(32)];
            for old in table.iter().flatten() {
                insert(&mut grown, *old);
            }
            table = grown;
        }
        insert(&mut table, *id);
    }
    table.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(first: u8, second: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[0] = first;
        bytes[1] = second;
        ObjectId::Sha1(bytes)
    }

    #[test]
    fn table_order_is_little_endian_slot_order_with_linear_probing() {
        // Size 32. Slots: 0x05 -> 5, 0x03 -> 3, 0x28 -> 8, then 0x01 -> 1 and 0x21 -> 1 collide,
        // so (inserted in sorted order) 0x01 keeps slot 1 and 0x21 moves to slot 2. A second byte
        // of 1 contributes 256, which is 0 modulo 32 and must not change the slot.
        let mut ids = vec![id(5, 1), id(3, 0), id(0x28, 0), id(1, 0), id(0x21, 0)];
        ids.sort();
        let order = git_table_order(&ids);
        assert_eq!(
            order,
            vec![id(1, 0), id(0x21, 0), id(3, 0), id(5, 1), id(0x28, 0)]
        );
    }

    #[test]
    fn table_doubles_once_it_would_pass_half_full() {
        // The 17th insert is the first to find 32 slots too few (16 * 2 >= 31) and moves to 64.
        // Slot = first byte % size, so ids up to 49 stay in ascending order only in 64 slots.
        let ids: Vec<ObjectId> = (0..17u8).map(|i| id(i * 3 + 1, 0)).collect();
        assert_eq!(git_table_order(&ids), ids);
        // Sixteen of them still use 32 slots: 34 and 37 wrap round to slots 2 and 5.
        let order = git_table_order(&ids[..16]);
        assert_eq!(order[..4], [id(1, 0), id(34, 0), id(4, 0), id(37, 0)]);
    }
}
