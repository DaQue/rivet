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
    let mut ids = odb.oids()?;
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
                out.extend(commit.parents.iter().map(|p| (ObjectKind::Commit, *p)));
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
    for oid in all {
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
