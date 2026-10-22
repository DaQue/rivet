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
    let mut reachable = HashSet::new();
    crate::remote::transport::collect_objects(odb, &roots, &HashSet::new(), &mut reachable)?;
    Ok(reachable)
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
/// them (tag objects, then commits newest first, then each commit's trees and blobs depth first),
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
                        out.push((oid, pack_name_hash(tag.name)));
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
        // Commits and tags first, then trees, then blobs; a delta only after its base.
        Layout::ByType => {
            for rank in [
                [ObjectKind::Tag, ObjectKind::Commit].as_slice(),
                [ObjectKind::Tree].as_slice(),
                [ObjectKind::Blob].as_slice(),
            ] {
                for (i, input) in inputs.iter().enumerate() {
                    if rank.contains(&input.kind) {
                        sequence.push(i);
                    }
                }
            }
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

/// `reflog expire --all`: drop entries older than the cutoffs, except for `refs/stash`.
fn expire_reflogs(repo: &Repository, options: &GcOptions) -> Result<usize> {
    if options.reflog_expire.is_none() && options.reflog_expire_unreachable.is_none() {
        return Ok(0);
    }
    let git_dir = repo.git_dir();
    let mut names = Vec::new();
    reflog_names(&git_dir.join("logs"), "", &mut names);
    names.sort();
    let mut dropped = 0;
    for text in names {
        if text == "refs/stash" {
            continue;
        }
        let Ok(name) = RefName::from_bytes(text.as_bytes()) else {
            continue;
        };
        let entries = repo.refs().reflog(&name)?;
        // Everything the ref's current commit reaches: what "reachable" means for an entry.
        let tip = repo
            .refs()
            .resolve_oid(&name)
            .ok()
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
    let git_dir = repo.git_dir();
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

/// Run garbage collection on `repo`.
///
/// # Errors
///
/// Any object that is reachable but unreadable aborts the run before a file is changed; I/O
/// errors while writing the new pack likewise leave the old objects in place.
pub fn gc(repo: &Repository, options: &GcOptions) -> Result<GcReport> {
    let mut report = GcReport::default();
    let git_dir = repo.git_dir();
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
        let order = recency_order(repo, &wanted, tips, &[])?;
        let summary = write_deltified(repo, order, &reusable, &pack_dir, Layout::ByType)?;
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
            let summary =
                write_deltified(repo, plan.order, &reusable, &pack_dir, Layout::AsListed)?;
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
