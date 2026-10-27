//! `gc`: pack refs, pack every reachable object into one pack, and prune what is unreachable.
//!
//! The order is what makes it safe: reachability is computed first and any unreadable object
//! aborts the run before anything is touched; the new pack is written, renamed into place and
//! verified before an old pack or a loose object is removed; unreachable objects that are not yet
//! old enough to prune survive (loose ones stay, ones inside a pack that is being replaced are
//! written back out loose, keeping the pack's age). Objects an unreachable object that is still
//! young needs are kept too, as `git prune` does.
//!
//! With [`GcOptions::cruft`] the young unreachable objects go into a cruft pack of their own
//! instead: `pack-<id>.pack` with a `.mtimes` file holding each object's age, written in the order
//! `git repack --cruft` uses, with the objects a young one reaches (their age set to the cutoff).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bstr::BString;

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::{ObjectKind, Tag};
use crate::pack::PackWriter;
use crate::refs::packed::{PackedRef, PackedRefs};
use crate::refs::{RefName, RefTarget};
use crate::repo::Repository;

/// What [`gc`] should do.
#[derive(Clone, Debug)]
pub struct GcOptions {
    /// Delete unreachable objects last modified at or before this time (seconds since the
    /// epoch); `None` keeps every unreachable object.
    pub prune_before: Option<i64>,
    /// Move every ref into `packed-refs` and delete the loose files.
    pub pack_refs: bool,
    /// Drop reflog entries older than this (`gc.reflogExpire`); `None` keeps them all.
    pub reflog_expire: Option<i64>,
    /// Drop reflog entries older than this whose commits are no longer reachable from the ref
    /// (`gc.reflogExpireUnreachable`); `None` keeps them.
    pub reflog_expire_unreachable: Option<i64>,
    /// Keep unreachable objects younger than `prune_before` in a separate cruft pack (with a
    /// `.mtimes` file recording each object's age) instead of leaving them loose.
    pub cruft: bool,
}

impl GcOptions {
    /// Prune nothing, pack refs, no cruft pack.
    pub fn new() -> Self {
        Self {
            prune_before: None,
            pack_refs: true,
            reflog_expire: None,
            reflog_expire_unreachable: None,
            cruft: false,
        }
    }
}

impl Default for GcOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// What a [`gc`] run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Refs written to `packed-refs`.
    pub refs_packed: usize,
    /// Reflog entries dropped.
    pub reflog_entries_expired: usize,
    /// Objects in the new pack (0 when the repository was already packed).
    pub objects_packed: usize,
    /// Old packs removed.
    pub packs_removed: usize,
    /// Loose objects removed because they are now in the pack.
    pub loose_removed: usize,
    /// Unreachable objects written back out of a removed pack.
    pub objects_unpacked: usize,
    /// Unreachable objects deleted.
    pub objects_pruned: usize,
    /// Objects in the new cruft pack (0 when none was written).
    pub cruft_objects: usize,
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::IoAtPath {
        path: BString::from(path.to_string_lossy().as_ref()),
        source,
    }
}

fn mtime_seconds(path: &Path) -> Result<i64> {
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| io_error(path, e))?;
    Ok(match modified.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    })
}

/// The per-object mtimes of a Git cruft pack, keyed by object id, if the file parses.
///
/// Layout: `MTME`, version 1, hash id, one big-endian u32 per object in index (sorted) order,
/// then two checksums. An unreadable file yields nothing and the pack's own mtime is used.
fn cruft_mtimes(path: &Path, oids: &[ObjectId]) -> Vec<(ObjectId, i64)> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    if data.len() < 12 || &data[..4] != b"MTME" {
        return Vec::new();
    }
    let mut sorted = oids.to_vec();
    sorted.sort();
    let table = &data[12..];
    if table.len() < sorted.len() * 4 {
        return Vec::new();
    }
    sorted
        .into_iter()
        .enumerate()
        .map(|(i, oid)| {
            let raw = [
                table[i * 4],
                table[i * 4 + 1],
                table[i * 4 + 2],
                table[i * 4 + 3],
            ];
            (oid, i64::from(u32::from_be_bytes(raw)))
        })
        .collect()
}

fn peel_fully(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let object = repo.odb().read(&oid)?;
        if object.kind() != ObjectKind::Tag {
            return Ok(oid);
        }
        oid = Tag::parse(repo.format(), object.data())?.object;
    }
}

/// Every object id reachable from refs, `HEAD`, the index and, if asked, reflogs.
fn reachable_objects(repo: &Repository, with_reflogs: bool) -> Result<HashSet<ObjectId>> {
    let odb = repo.odb();
    let mut roots: Vec<ObjectId> = Vec::new();
    let mut names: Vec<RefName> = Vec::new();
    for (name, target) in repo.refs().list(b"refs/")? {
        match target {
            RefTarget::Direct(oid) => roots.push(oid),
            RefTarget::Symbolic(_) => {}
        }
        names.push(name);
    }
    names.push(RefName::head());
    if let Some(oid) = repo.head()?.oid() {
        roots.push(oid);
    }
    // Reflogs keep objects alive, but an entry whose object is already gone is not an error.
    for name in names.iter().filter(|_| with_reflogs) {
        for entry in repo.refs().reflog(name)? {
            for oid in [entry.old, entry.new] {
                if !oid.is_zero() && odb.contains(&oid) {
                    roots.push(oid);
                }
            }
        }
    }
    for entry in repo.index().entries() {
        if entry.mode != crate::object::EntryMode::Commit && odb.contains(&entry.oid) {
            roots.push(entry.oid);
        }
    }
    roots.extend(other_worktree_roots(repo, with_reflogs)?);
    let mut reachable = HashSet::new();
    crate::remote::transport::collect_objects(odb, &roots, &HashSet::new(), &mut reachable)?;
    Ok(reachable)
}

/// What the other work trees of the repository keep alive: a detached `HEAD`, the blobs of the
/// index and, if asked, the entries of the `HEAD` reflog. Branches they are on are ordinary refs.
pub(crate) fn other_worktree_roots(repo: &Repository, with_reflogs: bool) -> Result<Vec<ObjectId>> {
    let odb = repo.odb();
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if repo.git_dir() != repo.common_dir() {
        dirs.push(repo.common_dir().to_path_buf());
    }
    if let Ok(read) = std::fs::read_dir(repo.common_dir().join("worktrees")) {
        let mut linked: Vec<std::path::PathBuf> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p != repo.git_dir())
            .collect();
        linked.sort();
        dirs.extend(linked);
    }
    let mut roots = Vec::new();
    for dir in dirs {
        if let Ok(text) = std::fs::read(dir.join("HEAD"))
            && !text.starts_with(b"ref:")
            && let Ok(oid) = ObjectId::from_hex(repo.format(), bstr::BStr::new(text.trim_ascii()))
            && odb.contains(&oid)
        {
            roots.push(oid);
        }
        let index_path = dir.join("index");
        if index_path.is_file() {
            let index = crate::index::Index::load(&index_path, repo.format())?;
            for entry in index.entries() {
                if entry.mode != crate::object::EntryMode::Commit && odb.contains(&entry.oid) {
                    roots.push(entry.oid);
                }
            }
        }
        if with_reflogs {
            for entry in crate::refs::reflog::read(&dir, &RefName::head(), repo.format())? {
                for oid in [entry.old, entry.new] {
                    if !oid.is_zero() && odb.contains(&oid) {
                        roots.push(oid);
                    }
                }
            }
        }
    }
    Ok(roots)
}

/// The starting points of the object walk in `--all --reflog` order: `HEAD`, the refs, then the
/// reflog entries. Must be read before refs are packed, as the ref store's snapshot goes stale
/// once the loose files are gone.
fn walk_tips(repo: &Repository) -> Result<Vec<(Vec<u8>, ObjectId)>> {
    let odb = repo.odb();
    let mut tips: Vec<(Vec<u8>, ObjectId)> = Vec::new();
    if let Some(oid) = repo.head()?.oid() {
        tips.push((b"HEAD".to_vec(), oid));
    }
    for (name, target) in repo.refs().list(b"refs/")? {
        if let RefTarget::Direct(oid) = target {
            tips.push((name.as_bytes().to_vec(), oid));
        }
    }
    let mut names: Vec<RefName> = vec![RefName::head()];
    for (name, _) in repo.refs().list(b"refs/")? {
        names.push(name);
    }
    for name in &names {
        for entry in repo.refs().reflog(name)? {
            for oid in [entry.old, entry.new] {
                if !oid.is_zero() && odb.contains(&oid) {
                    tips.push((Vec::new(), oid));
                }
            }
        }
    }
    Ok(tips)
}

/// The objects of `wanted` in the order `git pack-objects --all --reflog --indexed-objects` lists
/// them (commits newest first, then tag objects, then each commit's trees and blobs depth first),
/// each with Git's name hash of the first path it was reached by.
fn recency_order(
    repo: &Repository,
    wanted: &HashSet<ObjectId>,
    tips: Vec<(Vec<u8>, ObjectId)>,
    root_trees: &[ObjectId],
) -> Result<Vec<(ObjectId, u32)>> {
    use crate::object::{Commit, Tree};
    use crate::pack::deltify::pack_name_hash;
    use std::collections::BinaryHeap;

    let odb = repo.odb();
    let format = repo.format();
    let mut out: Vec<(ObjectId, u32)> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();

    // `HEAD` as the walk saw it: the ref store's own view is stale once refs are packed.
    let head_tip = tips
        .iter()
        .find(|(name, _)| name == b"HEAD")
        .map(|(_, oid)| *oid);
    // Git shows tag objects with the pending (non-commit) objects, after every commit.
    let mut tag_objects: Vec<(ObjectId, u32)> = Vec::new();
    let mut commit_tips: Vec<ObjectId> = Vec::new();
    for (_, mut oid) in tips {
        loop {
            if !odb.contains(&oid) {
                break;
            }
            let object = odb.read(&oid)?;
            match object.kind() {
                ObjectKind::Tag => {
                    // Git names a tag object by the `tag` header it carries.
                    let tag = Tag::parse(format, object.data())?;
                    if seen.insert(oid) && wanted.contains(&oid) {
                        tag_objects.push((oid, pack_name_hash(tag.name)));
                    }
                    oid = tag.object;
                }
                ObjectKind::Commit => {
                    commit_tips.push(oid);
                    break;
                }
                _ => break,
            }
        }
    }

    // Commits by date, newest first, a commit only after it was reached from a child.
    type Entry = (i64, std::cmp::Reverse<usize>, ObjectId);
    let mut heap: BinaryHeap<Entry> = BinaryHeap::new();
    let mut queued: HashSet<ObjectId> = HashSet::new();
    let mut sequence = 0usize;
    let mut push =
        |heap: &mut BinaryHeap<Entry>, oid: ObjectId, sequence: &mut usize| -> Result<()> {
            if !queued.insert(oid) || !odb.contains(&oid) {
                return Ok(());
            }
            let object = odb.read(&oid)?;
            let commit = Commit::parse(format, object.data())?;
            heap.push((commit.committer.when, std::cmp::Reverse(*sequence), oid));
            *sequence += 1;
            Ok(())
        };
    for oid in commit_tips {
        push(&mut heap, oid, &mut sequence)?;
    }
    let mut commits: Vec<(ObjectId, ObjectId)> = Vec::new();
    while let Some((_, _, oid)) = heap.pop() {
        let object = odb.read(&oid)?;
        let commit = Commit::parse(format, object.data())?;
        commits.push((oid, commit.tree));
        for parent in &commit.parents {
            push(&mut heap, *parent, &mut sequence)?;
        }
    }
    for (oid, _) in &commits {
        if seen.insert(*oid) && wanted.contains(oid) {
            out.push((*oid, 0));
        }
    }

    out.append(&mut tag_objects);

    // `--indexed-objects` puts the index's blobs in the pending list, ahead of any commit's tree.
    for entry in repo.index().entries() {
        if entry.mode != crate::object::EntryMode::Commit
            && seen.insert(entry.oid)
            && wanted.contains(&entry.oid)
        {
            out.push((entry.oid, crate::pack::deltify::pack_name_hash(&entry.path)));
        }
    }

    // Trees and blobs, commit by commit, depth first.
    fn walk(
        repo: &Repository,
        tree: ObjectId,
        path: &mut Vec<u8>,
        wanted: &HashSet<ObjectId>,
        seen: &mut HashSet<ObjectId>,
        out: &mut Vec<(ObjectId, u32)>,
    ) -> Result<()> {
        if !seen.insert(tree) {
            return Ok(());
        }
        if wanted.contains(&tree) {
            out.push((tree, crate::pack::deltify::pack_name_hash(path)));
        }
        let object = repo.odb().read(&tree)?;
        let parsed = Tree::parse(repo.format(), object.data())?;
        for entry in &parsed.entries {
            let base = path.len();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&entry.name);
            match entry.mode {
                crate::object::EntryMode::Tree => walk(repo, entry.oid, path, wanted, seen, out)?,
                crate::object::EntryMode::Commit => {}
                _ => {
                    if seen.insert(entry.oid) && wanted.contains(&entry.oid) {
                        out.push((entry.oid, crate::pack::deltify::pack_name_hash(path)));
                    }
                }
            }
            path.truncate(base);
        }
        Ok(())
    }
    // Trees named directly come first, as git's pending list has them before any commit's tree.
    for tree in root_trees {
        if odb.contains(tree) {
            walk(repo, *tree, &mut Vec::new(), wanted, &mut seen, &mut out)?;
        }
    }
    // The index's cached trees come next in the pending list, ahead of every commit's tree.
    if let Some(head) = head_tip
        && odb.contains(&head)
    {
        let tree = repo.commit_tree(head)?;
        for (mut path, oid) in cache_tree_dirs(repo, tree)? {
            walk(repo, oid, &mut path, wanted, &mut seen, &mut out)?;
        }
    }
    for (_, tree) in &commits {
        walk(repo, *tree, &mut Vec::new(), wanted, &mut seen, &mut out)?;
    }

    // Whatever else must be packed (objects only an index entry or reflog line reaches).
    let mut rest: Vec<ObjectId> = wanted
        .iter()
        .filter(|o| !seen.contains(*o))
        .copied()
        .collect();
    rest.sort();
    out.extend(rest.into_iter().map(|oid| (oid, 0)));
    Ok(out)
}

/// The order `git pack-objects` writes a pack in (`compute_write_order`), as indexes into the
/// object list: the untagged head of the list, the tagged tips, every remaining commit and tag,
/// every tree (an object stored as a delta in the packs being replaced counts as neither, as
/// git still sees it under its delta type), and last each still unwritten object together with the delta family it belongs to
/// (the family's root, then its descendants depth first, siblings in list order).
fn git_write_order(
    kinds: &[ObjectKind],
    bases: &[Option<usize>],
    tagged: &[bool],
    reused: &[bool],
) -> Vec<usize> {
    let n = kinds.len();
    let mut child: Vec<Option<usize>> = vec![None; n];
    let mut sibling: Vec<Option<usize>> = vec![None; n];
    for i in (0..n).rev() {
        if let Some(base) = bases[i] {
            sibling[i] = child[base];
            child[base] = Some(i);
        }
    }
    let mut out: Vec<usize> = Vec::with_capacity(n);
    let mut filled = vec![false; n];
    fn add(filled: &mut [bool], i: usize, out: &mut Vec<usize>) {
        if !filled[i] {
            filled[i] = true;
            out.push(i);
        }
    }
    let last_untagged = tagged.iter().position(|t| *t).unwrap_or(n);
    for i in 0..last_untagged {
        add(&mut filled, i, &mut out);
    }
    for (i, _) in tagged
        .iter()
        .enumerate()
        .skip(last_untagged)
        .filter(|(_, t)| **t)
    {
        add(&mut filled, i, &mut out);
    }
    for i in last_untagged..n {
        if !reused[i] && matches!(kinds[i], ObjectKind::Commit | ObjectKind::Tag) {
            add(&mut filled, i, &mut out);
        }
    }
    for i in last_untagged..n {
        if !reused[i] && kinds[i] == ObjectKind::Tree {
            add(&mut filled, i, &mut out);
        }
    }
    for i in last_untagged..n {
        if filled[i] {
            continue;
        }
        let mut root = i;
        while let Some(base) = bases[root] {
            root = base;
        }
        let mut start = Some(root);
        let mut add_to_order = true;
        while let Some(node) = start {
            if add_to_order {
                add(&mut filled, node, &mut out);
                let mut s = sibling[node];
                while let Some(next) = s {
                    add(&mut filled, next, &mut out);
                    s = sibling[next];
                }
            }
            if let Some(first) = child[node] {
                start = Some(first);
                add_to_order = true;
            } else {
                let mut cursor = Some(node);
                start = None;
                while let Some(c) = cursor {
                    if let Some(next) = sibling[c] {
                        start = Some(next);
                        add_to_order = false;
                        break;
                    }
                    cursor = bases[c];
                }
            }
        }
    }
    out
}

/// The directories of the index that git's cache-tree would hold as valid, in the order git adds
/// them to its pending list (a directory, then its subdirectories by name), each with its tree.
///
/// Rivet keeps no cache-tree, so a directory counts as valid when everything the index holds under
/// it is exactly what `head_tree` holds there (and nothing under it is unmerged); that is what git's
/// cache is right after a commit, a checkout or a reset, and it stays so for the directories an
/// `add` did not touch.
fn cache_tree_dirs(repo: &Repository, head_tree: ObjectId) -> Result<Vec<(Vec<u8>, ObjectId)>> {
    use crate::object::{EntryMode, Tree};

    type Files = BTreeMap<Vec<u8>, (EntryMode, ObjectId)>;
    fn collect(
        repo: &Repository,
        tree: ObjectId,
        path: &mut Vec<u8>,
        files: &mut Files,
        dirs: &mut HashMap<Vec<u8>, ObjectId>,
    ) -> Result<()> {
        dirs.insert(path.clone(), tree);
        let object = repo.odb().read(&tree)?;
        let parsed = Tree::parse(repo.format(), object.data())?;
        for entry in &parsed.entries {
            let base = path.len();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&entry.name);
            if entry.mode == EntryMode::Tree {
                collect(repo, entry.oid, path, files, dirs)?;
            } else {
                files.insert(path.clone(), (entry.mode, entry.oid));
            }
            path.truncate(base);
        }
        Ok(())
    }
    #[derive(Default)]
    struct Node {
        children: BTreeMap<Vec<u8>, Node>,
    }
    fn under<'a>(
        files: &'a Files,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (&'a Vec<u8>, &'a (EntryMode, ObjectId))> {
        files
            .range(prefix.to_vec()..)
            .take_while(move |(key, _)| key.starts_with(prefix))
    }
    struct Context<'a> {
        head: &'a Files,
        index: &'a Files,
        dirs: &'a HashMap<Vec<u8>, ObjectId>,
        poisoned: &'a [Vec<u8>],
    }
    fn visit(
        node: &Node,
        path: &mut Vec<u8>,
        context: &Context<'_>,
        out: &mut Vec<(Vec<u8>, ObjectId)>,
    ) {
        let mut prefix = path.clone();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let whole = under(context.head, &prefix).eq(under(context.index, &prefix))
            && !context.poisoned.iter().any(|p| p.starts_with(&prefix));
        if whole && let Some(oid) = context.dirs.get(path) {
            out.push((path.clone(), *oid));
        }
        for (name, child) in &node.children {
            let base = path.len();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            visit(child, path, context, out);
            path.truncate(base);
        }
    }

    let mut head: Files = BTreeMap::new();
    let mut dirs: HashMap<Vec<u8>, ObjectId> = HashMap::new();
    collect(repo, head_tree, &mut Vec::new(), &mut head, &mut dirs)?;
    let mut index: Files = BTreeMap::new();
    let mut poisoned: Vec<Vec<u8>> = Vec::new();
    let mut root = Node::default();
    for entry in repo.index().entries() {
        let path: Vec<u8> = entry.path.to_vec();
        if entry.is_unmerged() {
            poisoned.push(path.clone());
        } else {
            index.insert(path.clone(), (entry.mode, entry.oid));
        }
        let mut node = &mut root;
        let mut parts = path.split(|b| *b == b'/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            node = node.children.entry(part.to_vec()).or_default();
        }
    }
    let mut out = Vec::new();
    let context = Context {
        head: &head,
        index: &index,
        dirs: &dirs,
        poisoned: &poisoned,
    };
    visit(&root, &mut Vec::new(), &context, &mut out);
    Ok(out)
}

/// The order objects are written to a pack in.
#[derive(Clone, Copy)]
enum Layout {
    /// Commits and tags, then trees, then blobs (the pack of reachable objects).
    ByType,
    /// The order given, a delta's base written just before it (a cruft pack).
    AsListed,
}

/// Write `order` as one pack, deltified the way `git pack-objects` would: returns the pack's
/// summary. `reusable` maps an object to the delta it is already stored as in a pack that is
/// being replaced.
fn write_deltified(
    repo: &Repository,
    order: Vec<(ObjectId, u32)>,
    reusable: &HashMap<ObjectId, (ObjectId, Vec<u8>)>,
    pack_dir: &Path,
    layout: Layout,
    tag_refs: &[ObjectId],
) -> Result<crate::pack::writer::PackWriteSummary> {
    use crate::pack::deltify::{DeltaInput, search_deltas};

    let odb = repo.odb();
    let position: HashMap<ObjectId, usize> = order
        .iter()
        .enumerate()
        .map(|(i, (oid, _))| (*oid, i))
        .collect();
    let mut inputs: Vec<DeltaInput> = Vec::with_capacity(order.len());
    for (oid, hash) in &order {
        let object = odb.read(oid)?;
        let reused = reusable
            .get(oid)
            .and_then(|(base, delta)| position.get(base).map(|b| (*b, delta.len() as u64)));
        inputs.push(DeltaInput {
            kind: object.kind(),
            name_hash: *hash,
            size: object.data().len() as u64,
            reused_base: reused.map(|(b, _)| b),
            reused_size: reused.map(|(_, s)| s).unwrap_or(0),
        });
    }

    let config_number = |key: &str, default: usize| -> usize {
        repo.config_value(bstr::BStr::new(key.as_bytes()))
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8_lossy(&v).trim().parse::<usize>().ok())
            .unwrap_or(default)
    };
    let window = config_number("pack.window", 10);
    let depth = config_number("pack.depth", 50);
    let chosen = if window == 0 || depth == 0 {
        vec![None; order.len()]
    } else {
        let mut load = |i: usize| -> Result<Vec<u8>> { Ok(odb.read(&order[i].0)?.data().to_vec()) };
        search_deltas(
            &inputs,
            &mut load,
            window,
            depth,
            match repo.format() {
                crate::hash::ObjectFormat::Sha1 => 20,
                _ => 32,
            },
        )?
    };

    let mut sequence: Vec<usize> = Vec::with_capacity(order.len());
    match layout {
        // Git's `compute_write_order`: tagged tips, commits and tags, trees, then the rest by
        // delta family; a delta only after its base.
        Layout::ByType => {
            let mut tagged: HashSet<ObjectId> = HashSet::new();
            for oid in tag_refs {
                tagged.insert(*oid);
                if odb.contains(oid) {
                    tagged.insert(peel_fully(repo, *oid)?);
                }
            }
            let kinds: Vec<ObjectKind> = inputs.iter().map(|input| input.kind).collect();
            let bases: Vec<Option<usize>> =
                chosen.iter().map(|c| c.as_ref().map(|c| c.base)).collect();
            let is_tagged: Vec<bool> = order.iter().map(|(oid, _)| tagged.contains(oid)).collect();
            let reused: Vec<bool> = inputs.iter().map(|i| i.reused_base.is_some()).collect();
            sequence = git_write_order(&kinds, &bases, &is_tagged, &reused);
        }
        // In the order given; `emit` writes a delta's base first, wherever the base is listed.
        Layout::AsListed => sequence.extend(0..order.len()),
    }
    let mut writer = PackWriter::new(repo.format());
    let mut offsets: Vec<Option<u64>> = vec![None; order.len()];
    fn emit(
        i: usize,
        repo: &Repository,
        order: &[(ObjectId, u32)],
        chosen: &[Option<crate::pack::deltify::ChosenDelta>],
        reusable: &HashMap<ObjectId, (ObjectId, Vec<u8>)>,
        writer: &mut PackWriter,
        offsets: &mut [Option<u64>],
    ) -> Result<()> {
        if offsets[i].is_some() {
            return Ok(());
        }
        match &chosen[i] {
            Some(choice) => {
                emit(choice.base, repo, order, chosen, reusable, writer, offsets)?;
                let base_offset = offsets[choice.base].expect("base written first");
                let delta: &[u8] = match &choice.delta {
                    Some(bytes) => bytes,
                    None => &reusable[&order[i].0].1,
                };
                offsets[i] = Some(writer.write_ofs_delta(order[i].0, base_offset, delta)?);
            }
            None => {
                let object = repo.odb().read(&order[i].0)?;
                writer.write_raw(object.kind(), object.data())?;
                offsets[i] = writer.last_offset();
            }
        }
        Ok(())
    }
    for i in sequence {
        emit(
            i,
            repo,
            &order,
            &chosen,
            reusable,
            &mut writer,
            &mut offsets,
        )?;
    }
    let (_, summary) = writer.finish_to_path(pack_dir)?;
    write_rev(pack_dir, repo.format(), &summary)?;
    Ok(summary)
}

/// The objects of `candidates` that something in `from` reaches (and `from` itself is not).
///
/// `git prune` and `repack --cruft` both keep an old unreachable object that a newer unreachable
/// one still needs, so that the newer one stays complete.
fn reached_from(
    repo: &Repository,
    candidates: &HashMap<ObjectId, i64>,
    from: &[ObjectId],
) -> HashSet<ObjectId> {
    use crate::object::{Commit, EntryMode, Tree};

    let odb = repo.odb();
    let format = repo.format();
    let mut retained: HashSet<ObjectId> = HashSet::new();
    let mut seen: HashSet<ObjectId> = from.iter().copied().collect();
    let mut stack: Vec<ObjectId> = from.to_vec();
    while let Some(oid) = stack.pop() {
        let Ok(object) = odb.read(&oid) else {
            continue;
        };
        let mut children: Vec<ObjectId> = Vec::new();
        match object.kind() {
            ObjectKind::Commit => {
                if let Ok(commit) = Commit::parse(format, object.data()) {
                    children.push(commit.tree);
                    children.extend(commit.parents.iter().copied());
                }
            }
            ObjectKind::Tree => {
                if let Ok(tree) = Tree::parse(format, object.data()) {
                    children.extend(
                        tree.entries
                            .iter()
                            .filter(|e| e.mode != EntryMode::Commit)
                            .map(|e| e.oid),
                    );
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = Tag::parse(format, object.data()) {
                    children.push(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
        for child in children {
            if candidates.contains_key(&child) && seen.insert(child) {
                retained.insert(child);
                stack.push(child);
            }
        }
    }
    retained
}

/// The unreachable objects a cruft pack will hold, in pack order, with the age recorded for each.
struct CruftPlan {
    order: Vec<(ObjectId, u32)>,
    mtimes: HashMap<ObjectId, i64>,
}

/// Decide what the cruft pack keeps, as `git pack-objects --cruft --cruft-expiration` does.
///
/// `listed` has every unreachable object with its age, in the order git finds them: loose objects
/// by id, then each pack's objects in pack order, newest pack first. An object is kept when it is
/// newer than `cutoff`, and so is everything such an object reaches, however old (those are
/// recorded with the cutoff as their age). The newer ones are written in that order, then the ones
/// kept for connectivity in the order a walk from the newer commits finds them.
fn plan_cruft(
    repo: &Repository,
    listed: &[(ObjectId, i64)],
    cutoff: Option<i64>,
) -> Result<CruftPlan> {
    let odb = repo.odb();
    let expired = |when: i64| cutoff.is_some_and(|c| when <= c);
    let candidates: HashMap<ObjectId, i64> = listed.iter().copied().collect();
    let recent: Vec<ObjectId> = listed
        .iter()
        .filter(|(_, when)| !expired(*when))
        .map(|(oid, _)| *oid)
        .collect();
    let mut mtimes: HashMap<ObjectId, i64> = recent.iter().map(|o| (*o, candidates[o])).collect();
    let mut order: Vec<(ObjectId, u32)> = recent.iter().map(|oid| (*oid, 0)).collect();

    let Some(cutoff) = cutoff else {
        return Ok(CruftPlan { order, mtimes });
    };
    // Everything a newer object reaches among the old unreachable ones.
    let retained = reached_from(repo, &candidates, &recent);
    if retained.is_empty() {
        return Ok(CruftPlan { order, mtimes });
    }
    let mut tips: Vec<(Vec<u8>, ObjectId)> = Vec::new();
    let mut root_trees: Vec<ObjectId> = Vec::new();
    for oid in &recent {
        match odb.read_header(oid) {
            Ok((ObjectKind::Commit | ObjectKind::Tag, _)) => tips.push((Vec::new(), *oid)),
            Ok((ObjectKind::Tree, _)) => root_trees.push(*oid),
            _ => {}
        }
    }
    for (oid, hash) in recency_order(repo, &retained, tips, &root_trees)? {
        mtimes.insert(oid, cutoff);
        order.push((oid, hash));
    }
    Ok(CruftPlan { order, mtimes })
}

/// Write `pack-<checksum>.rev`, the reverse index git writes beside every pack: `RIDX`, version 1,
/// the hash id, for each object in pack order its position in the `.idx`, then the pack's checksum
/// and the file's own.
fn write_rev(
    pack_dir: &Path,
    format: crate::hash::ObjectFormat,
    summary: &crate::pack::writer::PackWriteSummary,
) -> Result<()> {
    let index = crate::pack::index::PackIndex::parse(&summary.index, format)?;
    let mut by_offset: Vec<(u64, u32)> = (0..index.len())
        .filter_map(|i| index.entry(i).map(|entry| (entry.offset, i as u32)))
        .collect();
    by_offset.sort();
    let mut data: Vec<u8> = b"RIDX".to_vec();
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(
        &(match format {
            crate::hash::ObjectFormat::Sha1 => 1u32,
            crate::hash::ObjectFormat::Sha256 => 2u32,
        })
        .to_be_bytes(),
    );
    for (_, position) in &by_offset {
        data.extend_from_slice(&position.to_be_bytes());
    }
    data.extend_from_slice(summary.checksum.as_bytes());
    let mut hasher = format.hasher();
    hasher.update(&data);
    data.extend_from_slice(hasher.finish()?.as_bytes());
    let path = pack_dir.join(format!("pack-{}.rev", summary.checksum.to_hex()));
    std::fs::write(&path, data).map_err(|e| io_error(&path, e))
}

/// Write `pack-<checksum>.mtimes`: `MTME`, version 1, the hash id, one big-endian age per object
/// in index order, then the pack's checksum and the file's own.
fn write_mtimes(
    pack_dir: &Path,
    format: crate::hash::ObjectFormat,
    pack_checksum: ObjectId,
    mtimes: &HashMap<ObjectId, i64>,
    members: &[ObjectId],
) -> Result<()> {
    let mut sorted: Vec<ObjectId> = members.to_vec();
    sorted.sort();
    let mut data: Vec<u8> = b"MTME".to_vec();
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(
        &(match format {
            crate::hash::ObjectFormat::Sha1 => 1u32,
            crate::hash::ObjectFormat::Sha256 => 2u32,
        })
        .to_be_bytes(),
    );
    for oid in &sorted {
        let when = mtimes
            .get(oid)
            .copied()
            .unwrap_or(0)
            .clamp(0, i64::from(u32::MAX));
        data.extend_from_slice(&(when as u32).to_be_bytes());
    }
    data.extend_from_slice(pack_checksum.as_bytes());
    let mut hasher = format.hasher();
    hasher.update(&data);
    data.extend_from_slice(hasher.finish()?.as_bytes());
    let path = pack_dir.join(format!("pack-{}.mtimes", pack_checksum.to_hex()));
    std::fs::write(&path, data).map_err(|e| io_error(&path, e))
}

/// Collect the names of every reflog under `logs/`.
fn reflog_names(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        if entry.path().is_dir() {
            reflog_names(&entry.path(), &format!("{name}/"), out);
        } else {
            out.push(name);
        }
    }
}

/// The commit the `HEAD` file in `dir` names, directly or through a branch.
fn head_file_oid(repo: &Repository, dir: &Path) -> Option<ObjectId> {
    let text = std::fs::read(dir.join("HEAD")).ok()?;
    let text = text.trim_ascii();
    match text.strip_prefix(b"ref:") {
        Some(target) => {
            let name = RefName::from_bytes(target.trim_ascii()).ok()?;
            repo.refs().resolve_oid(&name).ok()
        }
        None => ObjectId::from_hex(repo.format(), bstr::BStr::new(text)).ok(),
    }
}

/// `reflog expire --all`: drop entries older than the cutoffs, except for `refs/stash`.
fn expire_reflogs(repo: &Repository, options: &GcOptions) -> Result<usize> {
    if options.reflog_expire.is_none() && options.reflog_expire_unreachable.is_none() {
        return Ok(0);
    }
    let common = repo.common_dir();
    let mut names = Vec::new();
    reflog_names(&common.join("logs"), "", &mut names);
    names.sort();
    // Every log with the directory it lives in: the shared ones, and the `HEAD` log of each
    // linked work tree (the main work tree's is `logs/HEAD` of the common directory).
    let mut logs: Vec<(PathBuf, String)> = names
        .into_iter()
        .map(|text| (common.to_path_buf(), text))
        .collect();
    if let Ok(read) = std::fs::read_dir(common.join("worktrees")) {
        let mut linked: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
        linked.sort();
        for admin in linked {
            if admin.join("logs/HEAD").is_file() {
                logs.push((admin, "HEAD".to_string()));
            }
        }
    }
    let mut dropped = 0;
    for (git_dir, text) in logs {
        let git_dir = git_dir.as_path();
        if text == "refs/stash" {
            continue;
        }
        let Ok(name) = RefName::from_bytes(text.as_bytes()) else {
            continue;
        };
        let entries = crate::refs::reflog::read(git_dir, &name, repo.format())?;
        // Everything the ref's current commit reaches: what "reachable" means for an entry.
        let tip = if name.is_head() {
            head_file_oid(repo, git_dir)
        } else {
            repo.refs().resolve_oid(&name).ok()
        }
        .and_then(|oid| repo.refs().peel_oid(oid, repo.odb()).ok());
        let mut ancestors: Option<HashSet<ObjectId>> = None;
        let mut reachable = |oid: &ObjectId| -> bool {
            if oid.is_zero() {
                return true;
            }
            let Some(tip) = tip else {
                return true;
            };
            let set = ancestors.get_or_insert_with(|| {
                // Git marks what the tip reaches but stops at commits older than the
                // unreachable-expiry cutoff: those, and everything behind them, count as
                // unreachable for the entries old enough to be expired.
                let limit = options.reflog_expire_unreachable.unwrap_or(i64::MIN);
                let mut seen: HashSet<ObjectId> = HashSet::from([tip]);
                let mut stack = vec![tip];
                while let Some(id) = stack.pop() {
                    let Ok(object) = repo.odb().read_typed(&id, ObjectKind::Commit) else {
                        continue;
                    };
                    let Ok(commit) = crate::object::Commit::parse(repo.format(), object.data())
                    else {
                        continue;
                    };
                    for parent in commit.parents {
                        if seen.contains(&parent) {
                            continue;
                        }
                        let Ok(object) = repo.odb().read_typed(&parent, ObjectKind::Commit) else {
                            continue;
                        };
                        let Ok(parsed) = crate::object::Commit::parse(repo.format(), object.data())
                        else {
                            continue;
                        };
                        if parsed.committer.when < limit {
                            continue;
                        }
                        seen.insert(parent);
                        stack.push(parent);
                    }
                }
                seen
            });
            set.contains(oid)
        };
        let mut kept = Vec::new();
        for entry in &entries {
            let when = entry.who.when;
            let expired = options.reflog_expire.is_some_and(|cutoff| when < cutoff)
                || options.reflog_expire_unreachable.is_some_and(|cutoff| {
                    when < cutoff && (!reachable(&entry.old) || !reachable(&entry.new))
                });
            if expired {
                dropped += 1;
            } else {
                kept.push(entry.clone());
            }
        }
        if kept.len() != entries.len() {
            let bytes: Vec<u8> = kept
                .iter()
                .flat_map(crate::refs::reflog::ReflogEntry::to_bytes)
                .collect();
            let path = name.reflog_path(git_dir);
            std::fs::write(&path, bytes).map_err(|e| io_error(&path, e))?;
        }
    }
    Ok(dropped)
}

fn pack_refs(repo: &Repository) -> Result<usize> {
    let git_dir = repo.common_dir();
    let mut packed = PackedRefs::new();
    packed.set_traits(vec![
        BString::from("peeled"),
        BString::from("fully-peeled"),
        BString::from("sorted"),
    ]);
    let mut loose_files: Vec<PathBuf> = Vec::new();
    for (name, target) in repo.refs().list(b"refs/")? {
        let RefTarget::Direct(oid) = target else {
            continue;
        };
        let peeled = match repo.odb().read_header(&oid) {
            Ok((ObjectKind::Tag, _)) => Some(peel_fully(repo, oid)?),
            _ => None,
        };
        let path = name.file_path(git_dir);
        if path.is_file() {
            loose_files.push(path);
        }
        packed.insert(PackedRef {
            name,
            target: oid,
            peeled,
        });
    }
    packed.sort();
    let count = packed.len();
    packed.write(git_dir, repo.format())?;
    for path in loose_files {
        let _ = std::fs::remove_file(&path);
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == git_dir.join("refs")
                || d == git_dir.join("refs/heads")
                || d == git_dir.join("refs/tags")
                || std::fs::remove_dir(d).is_err()
            {
                break;
            }
            dir = d.parent();
        }
    }
    Ok(count)
}

/// Whether `extensions.preciousObjects` is on: objects may then be added but never removed.
fn precious_objects(repo: &Repository) -> Result<bool> {
    match repo.config_value(bstr::BStr::new(b"extensions.preciousObjects"))? {
        Some(value) => crate::config::parse_bool(bstr::BStr::new(&value[..])),
        None => Ok(false),
    }
}

/// Run garbage collection on `repo`.
///
/// # Errors
///
/// Any object that is reachable but unreadable aborts the run before a file is changed; I/O
/// errors while writing the new pack likewise leave the old objects in place.
pub fn gc(repo: &Repository, options: &GcOptions) -> Result<GcReport> {
    let mut report = GcReport::default();
    let git_dir = repo.common_dir();
    let format = repo.format();
    let odb = repo.odb();
    let objects_dir = odb.objects_dir().to_path_buf();
    let pack_dir = objects_dir.join("pack");

    // Refs, `HEAD` and the index must all be readable before anything is changed.
    reachable_objects(repo, false)?;
    report.reflog_entries_expired = expire_reflogs(repo, options)?;
    // Before packing refs: the store's snapshot goes stale once the loose files are deleted.
    let reachable = reachable_objects(repo, true)?;
    let tips = walk_tips(repo)?;
    if options.pack_refs {
        report.refs_packed = pack_refs(repo)?;
    }
    // `extensions.preciousObjects`: git's gc still packs refs and expires reflogs, but neither
    // repacks nor prunes, so no pack or loose object is ever deleted.
    if precious_objects(repo)? {
        return Ok(report);
    }

    // Classify packs: `.keep` packs are left alone and their objects are not repacked.
    let mut keep_objects: HashSet<ObjectId> = HashSet::new();
    let mut removable: Vec<(PathBuf, ObjectId, Vec<ObjectId>, i64)> = Vec::new();
    let mut cruft: HashMap<ObjectId, i64> = HashMap::new();
    let mut cruft_packs: HashSet<ObjectId> = HashSet::new();
    for pack in odb.packs() {
        let stem = format!("pack-{}", pack.checksum());
        let pack_path = pack_dir.join(format!("{stem}.pack"));
        let oids = pack.oids()?;
        if pack_dir.join(format!("{stem}.keep")).exists() {
            keep_objects.extend(oids);
        } else {
            let mtime = mtime_seconds(&pack_path)?;
            // A cruft pack (written by Git) records each object's own mtime beside it.
            let own = cruft_mtimes(&pack_dir.join(format!("{stem}.mtimes")), &oids);
            if pack_dir.join(format!("{stem}.mtimes")).exists() {
                cruft_packs.insert(pack.checksum());
            }
            for (oid, when) in own {
                cruft.insert(oid, when);
            }
            removable.push((pack_dir.join(&stem), pack.checksum(), oids, mtime));
        }
    }

    let loose_ids: Vec<ObjectId> = odb.loose().oids()?;
    let mut to_pack: Vec<ObjectId> = reachable
        .iter()
        .filter(|oid| !keep_objects.contains(oid))
        .copied()
        .collect();
    to_pack.sort();

    // Already one pack holding exactly the reachable objects, and nothing loose is reachable. With
    // cruft packs the old ones are inputs to the new cruft pack, not part of the comparison.
    let regular: Vec<&(PathBuf, ObjectId, Vec<ObjectId>, i64)> = removable
        .iter()
        .filter(|(_, checksum, _, _)| !(options.cruft && cruft_packs.contains(checksum)))
        .collect();
    let already_packed = regular.len() == 1
        && loose_ids.iter().all(|oid| !reachable.contains(oid))
        && regular[0].2.len() == to_pack.len()
        && regular[0].2.iter().all(|oid| reachable.contains(oid));
    let kept_pack = if options.cruft && already_packed {
        Some(regular[0].1)
    } else {
        None
    };

    // Objects that live only in packs about to disappear and are not reachable.
    let mut unreachable_packed: BTreeMap<ObjectId, i64> = BTreeMap::new();
    if !already_packed && !options.cruft {
        for (_, _, oids, mtime) in &removable {
            for oid in oids {
                if !reachable.contains(oid) {
                    let when = cruft.get(oid).copied().unwrap_or(*mtime);
                    let entry = unreachable_packed.entry(*oid).or_insert(when);
                    *entry = (*entry).max(when);
                }
            }
        }
    }

    let mut new_checksum: Option<ObjectId> = None;
    if !already_packed && !to_pack.is_empty() {
        let wanted: HashSet<ObjectId> = to_pack.iter().copied().collect();
        // Deltas already stored in the packs being replaced are kept when their base is packed too.
        let mut reusable: HashMap<ObjectId, (ObjectId, Vec<u8>)> = HashMap::new();
        for pack in odb.packs() {
            if !removable
                .iter()
                .any(|(_, checksum, _, _)| *checksum == pack.checksum())
            {
                continue;
            }
            let by_offset = pack.oids_by_offset();
            for oid in pack.oids()? {
                if wanted.contains(&oid)
                    && !reusable.contains_key(&oid)
                    && let Some((base, delta)) = pack.stored_delta(&oid, &by_offset)?
                    && wanted.contains(&base)
                {
                    reusable.insert(oid, (base, delta));
                }
            }
        }
        let tag_refs: Vec<ObjectId> = tips
            .iter()
            .filter(|(name, _)| name.starts_with(b"refs/tags/"))
            .map(|(_, oid)| *oid)
            .collect();
        let order = recency_order(repo, &wanted, tips, &[])?;
        let summary =
            write_deltified(repo, order, &reusable, &pack_dir, Layout::ByType, &tag_refs)?;
        new_checksum = Some(summary.checksum);
        report.objects_packed = to_pack.len();

        // Verify what was written before anything else is removed.
        let check = crate::odb::Odb::open(git_dir, format)?;
        let store = check
            .packs()
            .iter()
            .find(|p| p.checksum() == summary.checksum)
            .ok_or_else(|| Error::Message("gc: the new pack was not found after writing".into()))?;
        store.verify_checksum()?;
        for oid in &to_pack {
            if !store.contains(oid) {
                return Err(Error::Message(format!(
                    "gc: object {oid} is missing from the new pack"
                )));
            }
        }
    }

    if already_packed && !options.cruft {
        return finish_loose(repo, options, &reachable, &mut report);
    }

    // The unreachable objects that are young enough, in a pack of their own.
    let mut new_cruft: Option<ObjectId> = None;
    let mut cruft_members: HashSet<ObjectId> = HashSet::new();
    if options.cruft {
        let mut listed: Vec<(ObjectId, i64)> = Vec::new();
        let mut position: HashMap<ObjectId, usize> = HashMap::new();
        let mut note = |oid: ObjectId, when: i64| match position.get(&oid) {
            Some(at) => listed[*at].1 = listed[*at].1.max(when),
            None => {
                position.insert(oid, listed.len());
                listed.push((oid, when));
            }
        };
        let mut loose_sorted = loose_ids.clone();
        loose_sorted.sort();
        let mut doomed: Vec<(&PathBuf, ObjectId, i64)> = removable
            .iter()
            .map(|(stem, checksum, _, mtime)| (stem, *checksum, *mtime))
            .collect();
        // Newest pack first; git lists equally old packs by descending name.
        doomed.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| b.0.cmp(a.0)));
        // With an expiry git looks at loose objects before packs, without one after them.
        for loose_pass in [
            options.prune_before.is_some(),
            options.prune_before.is_none(),
        ] {
            if loose_pass {
                for oid in &loose_sorted {
                    if !reachable.contains(oid) && !keep_objects.contains(oid) {
                        note(*oid, mtime_seconds(&odb.loose().path_for(oid))?);
                    }
                }
                continue;
            }
            for (_, checksum, mtime) in &doomed {
                let Some(pack) = odb.packs().iter().find(|p| p.checksum() == *checksum) else {
                    continue;
                };
                let mut in_pack_order: Vec<(u64, ObjectId)> =
                    pack.oids_by_offset().into_iter().collect();
                in_pack_order.sort();
                for (_, oid) in in_pack_order {
                    if !reachable.contains(&oid) && !keep_objects.contains(&oid) {
                        note(oid, cruft.get(&oid).copied().unwrap_or(*mtime));
                    }
                }
            }
        }
        let plan = plan_cruft(repo, &listed, options.prune_before)?;
        if !plan.order.is_empty() {
            let wanted: HashSet<ObjectId> = plan.order.iter().map(|(oid, _)| *oid).collect();
            let mut reusable: HashMap<ObjectId, (ObjectId, Vec<u8>)> = HashMap::new();
            for pack in odb.packs() {
                if !removable
                    .iter()
                    .any(|(_, checksum, _, _)| *checksum == pack.checksum())
                {
                    continue;
                }
                let by_offset = pack.oids_by_offset();
                for oid in pack.oids()? {
                    if wanted.contains(&oid)
                        && !reusable.contains_key(&oid)
                        && let Some((base, delta)) = pack.stored_delta(&oid, &by_offset)?
                        && wanted.contains(&base)
                    {
                        reusable.insert(oid, (base, delta));
                    }
                }
            }
            let members: Vec<ObjectId> = plan.order.iter().map(|(oid, _)| *oid).collect();
            let summary = write_deltified(
                repo,
                plan.order,
                &reusable,
                &pack_dir,
                Layout::AsListed,
                &[],
            )?;
            write_mtimes(&pack_dir, format, summary.checksum, &plan.mtimes, &members)?;
            let check = crate::odb::Odb::open(git_dir, format)?;
            let store = check
                .packs()
                .iter()
                .find(|p| p.checksum() == summary.checksum)
                .ok_or_else(|| {
                    Error::Message("gc: the new cruft pack was not found after writing".into())
                })?;
            store.verify_checksum()?;
            for oid in &members {
                if !store.contains(oid) {
                    return Err(Error::Message(format!(
                        "gc: object {oid} is missing from the new cruft pack"
                    )));
                }
            }
            report.cruft_objects = members.len();
            cruft_members = members.into_iter().collect();
            new_cruft = Some(summary.checksum);
        }
    }

    // Unreachable objects young enough to survive are written back out as loose objects, and so
    // are the old ones a young one still needs.
    let mut needed: HashSet<ObjectId> = HashSet::new();
    if let Some(cutoff) = options.prune_before
        && !options.cruft
    {
        let mut ages: HashMap<ObjectId, i64> = unreachable_packed
            .iter()
            .map(|(oid, when)| (*oid, *when))
            .collect();
        for oid in loose_ids.iter().filter(|oid| !reachable.contains(oid)) {
            let when = mtime_seconds(&odb.loose().path_for(oid))?;
            let entry = ages.entry(*oid).or_insert(when);
            *entry = (*entry).max(when);
        }
        let recent: Vec<ObjectId> = ages
            .iter()
            .filter(|(_, when)| **when > cutoff)
            .map(|(oid, _)| *oid)
            .collect();
        needed = reached_from(repo, &ages, &recent);
    }
    for (oid, mtime) in &unreachable_packed {
        let expired =
            options.prune_before.is_some_and(|cutoff| *mtime <= cutoff) && !needed.contains(oid);
        if expired || odb.loose().contains(oid) {
            continue;
        }
        let object = odb.read(oid)?;
        odb.loose().write_raw(object.kind(), object.data())?;
        let path = odb.loose().path_for(oid);
        if let Ok(file) = std::fs::File::options().write(true).open(&path) {
            let when =
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs((*mtime).max(0) as u64);
            let _ = file.set_modified(when);
        }
        report.objects_unpacked += 1;
    }

    // Only now are the replaced packs and the packed loose objects dropped.
    for (stem, checksum, _, _) in &removable {
        if Some(*checksum) == new_checksum
            || Some(*checksum) == new_cruft
            || Some(*checksum) == kept_pack
        {
            continue;
        }
        for extension in ["pack", "idx", "rev", "bitmap", "mtimes"] {
            let path = stem.with_extension(extension);
            if path.exists() {
                std::fs::remove_file(&path).map_err(|e| io_error(&path, e))?;
            }
        }
        report.packs_removed += 1;
    }
    for oid in &loose_ids {
        if reachable.contains(oid) || cruft_members.contains(oid) {
            odb.loose().remove(oid)?;
            report.loose_removed += 1;
        }
    }

    finish_loose(repo, options, &reachable, &mut report)
}

/// Prune unreachable loose objects older than the cutoff and drop empty fan-out directories.
fn finish_loose(
    repo: &Repository,
    options: &GcOptions,
    reachable: &HashSet<ObjectId>,
    report: &mut GcReport,
) -> Result<GcReport> {
    let loose = repo.odb().loose();
    if let Some(cutoff) = options.prune_before {
        let mut ages: HashMap<ObjectId, i64> = HashMap::new();
        for oid in loose.oids()? {
            if !reachable.contains(&oid) {
                ages.insert(oid, mtime_seconds(&loose.path_for(&oid))?);
            }
        }
        let recent: Vec<ObjectId> = ages
            .iter()
            .filter(|(_, when)| **when > cutoff)
            .map(|(oid, _)| *oid)
            .collect();
        let needed = reached_from(repo, &ages, &recent);
        for (oid, mtime) in ages {
            if mtime <= cutoff && !needed.contains(&oid) {
                loose.remove(&oid)?;
                report.objects_pruned += 1;
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(loose.objects_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }
    Ok(report.clone())
}

/// What [`repack`] should do: the flags of `git repack`.
#[derive(Clone, Debug, Default)]
pub struct RepackOptions {
    /// `-a`: pack every reachable object into one new pack, not only the loose ones.
    pub all: bool,
    /// `-A`: like `all`, and objects only a replaced pack held that nothing reaches are written
    /// back out loose rather than lost.
    pub loosen_unreachable: bool,
    /// `-d`: remove the packs the new one replaces and the loose objects now held by a pack.
    pub delete_redundant: bool,
    /// `-k`: with `all`, also pack the unreachable objects of the replaced packs and loose ones.
    pub keep_unreachable: bool,
    /// `-f` / `-F`: compute every delta again instead of reusing the ones stored in a pack.
    pub no_reuse_delta: bool,
}

/// What a [`repack`] run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepackReport {
    /// Objects in the new pack (0 when there was nothing new to pack).
    pub objects_packed: usize,
    /// Old packs removed.
    pub packs_removed: usize,
    /// Loose objects removed because a pack holds them.
    pub loose_removed: usize,
    /// Unreachable objects written back out loose.
    pub objects_unpacked: usize,
}

/// `git repack`: write a pack of the objects the options select, and with `delete_redundant`
/// drop what the new pack replaces.
///
/// Without `all` or `loosen_unreachable` only the reachable objects that are in no pack yet go
/// into the new pack and nothing is replaced; with either, every reachable object of every pack
/// that is not marked `.keep` is repacked. Reachability follows `HEAD`, all refs, reflogs and the
/// index, as `git repack` passes `--all --reflog --indexed-objects`.
///
/// # Errors
///
/// Any object that is reachable but unreadable aborts the run before a file is changed; I/O
/// errors while writing the new pack likewise leave the old objects in place.
pub fn repack(repo: &Repository, options: &RepackOptions) -> Result<RepackReport> {
    if options.delete_redundant && precious_objects(repo)? {
        return Err(Error::Message(
            "cannot delete packs in a precious-objects repo".to_string(),
        ));
    }
    let mut report = RepackReport::default();
    let git_dir = repo.common_dir();
    let format = repo.format();
    let odb = repo.odb();
    let pack_dir = odb.objects_dir().join("pack");
    let all_mode = options.all || options.loosen_unreachable;

    let reachable = reachable_objects(repo, true)?;
    let tips = walk_tips(repo)?;

    let mut keep_objects: HashSet<ObjectId> = HashSet::new();
    let mut in_any_pack: HashSet<ObjectId> = HashSet::new();
    let mut removable: Vec<(PathBuf, ObjectId, Vec<ObjectId>, i64)> = Vec::new();
    for pack in odb.packs() {
        let stem = format!("pack-{}", pack.checksum());
        let oids = pack.oids()?;
        in_any_pack.extend(oids.iter().copied());
        if pack_dir.join(format!("{stem}.keep")).exists() {
            keep_objects.extend(oids);
        } else {
            let mtime = mtime_seconds(&pack_dir.join(format!("{stem}.pack")))?;
            removable.push((pack_dir.join(&stem), pack.checksum(), oids, mtime));
        }
    }
    let loose_ids: Vec<ObjectId> = odb.loose().oids()?;

    let mut to_pack: Vec<ObjectId> = if all_mode {
        reachable
            .iter()
            .filter(|oid| !keep_objects.contains(oid))
            .copied()
            .collect()
    } else {
        reachable
            .iter()
            .filter(|oid| !in_any_pack.contains(oid))
            .copied()
            .collect()
    };
    to_pack.sort();
    to_pack.dedup();
    // `-k`: the unreachable objects follow the reachable ones, those of the replaced packs first
    // (newest pack first, each in the order it stores them), then the loose ones by object id.
    let mut unreachable_tail: Vec<ObjectId> = Vec::new();
    if all_mode && options.keep_unreachable {
        let mut listed: HashSet<ObjectId> = HashSet::new();
        // Git's pack list is the directory listing, newest pack first; a stable sort keeps the
        // listing's own order for packs written within the same second.
        let listing: Vec<PathBuf> = std::fs::read_dir(&pack_dir)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        let mut packs: Vec<&(PathBuf, ObjectId, Vec<ObjectId>, i64)> = removable.iter().collect();
        packs.sort_by_key(|(stem, _, _, mtime)| {
            let at = listing
                .iter()
                .position(|p| *p == stem.with_extension("idx"))
                .unwrap_or(usize::MAX);
            (std::cmp::Reverse(*mtime), at)
        });
        for (_, checksum, _, _) in packs {
            let Some(pack) = odb.packs().iter().find(|p| p.checksum() == *checksum) else {
                continue;
            };
            let mut in_pack_order: Vec<(u64, ObjectId)> =
                pack.oids_by_offset().into_iter().collect();
            in_pack_order.sort();
            for (_, oid) in in_pack_order {
                if !reachable.contains(&oid) && listed.insert(oid) {
                    unreachable_tail.push(oid);
                }
            }
        }
        let mut loose_sorted = loose_ids.clone();
        loose_sorted.sort();
        for oid in loose_sorted {
            if !reachable.contains(&oid) && !keep_objects.contains(&oid) && listed.insert(oid) {
                unreachable_tail.push(oid);
            }
        }
    }

    let mut new_checksum: Option<ObjectId> = None;
    if !to_pack.is_empty() || !unreachable_tail.is_empty() {
        let wanted: HashSet<ObjectId> = to_pack
            .iter()
            .chain(unreachable_tail.iter())
            .copied()
            .collect();
        let mut reusable: HashMap<ObjectId, (ObjectId, Vec<u8>)> = HashMap::new();
        if all_mode && !options.no_reuse_delta {
            for pack in odb.packs() {
                if !removable
                    .iter()
                    .any(|(_, checksum, _, _)| *checksum == pack.checksum())
                {
                    continue;
                }
                let by_offset = pack.oids_by_offset();
                for oid in pack.oids()? {
                    if wanted.contains(&oid)
                        && !reusable.contains_key(&oid)
                        && let Some((base, delta)) = pack.stored_delta(&oid, &by_offset)?
                        && wanted.contains(&base)
                    {
                        reusable.insert(oid, (base, delta));
                    }
                }
            }
        }
        let tag_refs: Vec<ObjectId> = tips
            .iter()
            .filter(|(name, _)| name.starts_with(b"refs/tags/"))
            .map(|(_, oid)| *oid)
            .collect();
        let reachable_part: HashSet<ObjectId> = to_pack.iter().copied().collect();
        let mut order = recency_order(repo, &reachable_part, tips, &[])?;
        order.extend(unreachable_tail.iter().map(|oid| (*oid, 0)));
        let summary =
            write_deltified(repo, order, &reusable, &pack_dir, Layout::ByType, &tag_refs)?;
        new_checksum = Some(summary.checksum);
        report.objects_packed = wanted.len();

        let check = crate::odb::Odb::open(git_dir, format)?;
        let store = check
            .packs()
            .iter()
            .find(|p| p.checksum() == summary.checksum)
            .ok_or_else(|| {
                Error::Message("repack: the new pack was not found after writing".into())
            })?;
        store.verify_checksum()?;
        for oid in &wanted {
            if !store.contains(oid) {
                return Err(Error::Message(format!(
                    "repack: object {oid} is missing from the new pack"
                )));
            }
        }
    }

    // Unreachable objects of the packs being replaced come back as loose objects (`-A`).
    if options.loosen_unreachable && !options.keep_unreachable {
        let mut unreachable_packed: BTreeMap<ObjectId, i64> = BTreeMap::new();
        for (_, _, oids, mtime) in &removable {
            for oid in oids.iter().filter(|oid| !reachable.contains(oid)) {
                let entry = unreachable_packed.entry(*oid).or_insert(*mtime);
                *entry = (*entry).max(*mtime);
            }
        }
        for (oid, mtime) in &unreachable_packed {
            if odb.loose().contains(oid) {
                continue;
            }
            let object = odb.read(oid)?;
            odb.loose().write_raw(object.kind(), object.data())?;
            let path = odb.loose().path_for(oid);
            if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                let when =
                    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs((*mtime).max(0) as u64);
                let _ = file.set_modified(when);
            }
            report.objects_unpacked += 1;
        }
    }

    if options.delete_redundant {
        if all_mode {
            for (stem, checksum, _, _) in &removable {
                if Some(*checksum) == new_checksum {
                    continue;
                }
                for extension in ["pack", "idx", "rev", "bitmap", "mtimes"] {
                    let path = stem.with_extension(extension);
                    if path.exists() {
                        std::fs::remove_file(&path).map_err(|e| io_error(&path, e))?;
                    }
                }
                report.packs_removed += 1;
            }
        }
        // `git prune-packed`: a loose object some pack holds is redundant.
        let after = crate::odb::Odb::open(git_dir, format)?;
        for oid in &loose_ids {
            if after.packs().iter().any(|p| p.contains(oid)) {
                odb.loose().remove(oid)?;
                report.loose_removed += 1;
            }
        }
        if let Ok(entries) = std::fs::read_dir(odb.loose().objects_dir()) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_order_puts_tagged_tips_then_commits_trees_and_delta_families() {
        use ObjectKind::{Blob, Commit, Tag, Tree};
        // 0 commit, 1 tagged commit, 2 its tag, 3 commit, 4 tree, then a delta family: 5 is the
        // root, 6 and 8 hang off it (6 first in list order), 7 hangs off 6; 9 stands alone.
        let kinds = [
            Commit, Commit, Tag, Commit, Tree, Blob, Blob, Blob, Blob, Blob,
        ];
        let bases = [
            None,
            None,
            None,
            None,
            None,
            None,
            Some(5),
            Some(6),
            Some(5),
            None,
        ];
        let tagged = [
            false, true, true, false, false, false, false, false, false, false,
        ];
        let reused = [false; 10];
        assert_eq!(
            git_write_order(&kinds, &bases, &tagged, &reused),
            vec![0, 1, 2, 3, 4, 5, 6, 8, 7, 9]
        );
    }

    #[test]
    fn write_order_without_tags_keeps_list_order_and_defers_reused_deltas() {
        use ObjectKind::{Commit, Tree};
        // No tagged tip: everything up to the end of the list is written as listed. With a tag
        // at the end, the untagged head stays as listed and a reused tree delta waits for its family.
        let kinds = [Commit, Tree, Tree, Tree, Commit];
        let bases = [None, None, Some(3), None, None];
        let tagged = [false, false, false, false, true];
        let reused = [false, false, true, false, false];
        assert_eq!(
            git_write_order(&kinds, &bases, &tagged, &reused),
            vec![0, 1, 2, 3, 4]
        );
        let tagged = [true, false, false, false, false];
        assert_eq!(
            git_write_order(&kinds, &bases, &tagged, &reused),
            vec![0, 4, 1, 3, 2]
        );
    }
}
