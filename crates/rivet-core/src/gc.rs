//! `gc`: pack refs, pack every reachable object into one pack, and prune what is unreachable.
//!
//! The order is what makes it safe: reachability is computed first and any unreadable object
//! aborts the run before anything is touched; the new pack is written, renamed into place and
//! verified before an old pack or a loose object is removed; unreachable objects that are not yet
//! old enough to prune survive (loose ones stay, ones inside a pack that is being replaced are
//! written back out loose, keeping the pack's age).

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
}

impl GcOptions {
    /// Prune nothing, pack refs.
    pub fn new() -> Self {
        Self {
            prune_before: None,
            pack_refs: true,
            reflog_expire: None,
            reflog_expire_unreachable: None,
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
    if options.pack_refs {
        report.refs_packed = pack_refs(repo)?;
    }

    // Classify packs: `.keep` packs are left alone and their objects are not repacked.
    let mut keep_objects: HashSet<ObjectId> = HashSet::new();
    let mut removable: Vec<(PathBuf, ObjectId, Vec<ObjectId>, i64)> = Vec::new();
    let mut cruft: HashMap<ObjectId, i64> = HashMap::new();
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

    // Already one pack holding exactly the reachable objects, and nothing loose is reachable.
    let already_packed = removable.len() == 1
        && loose_ids.iter().all(|oid| !reachable.contains(oid))
        && removable[0].2.len() == to_pack.len()
        && removable[0].2.iter().all(|oid| reachable.contains(oid));

    // Objects that live only in packs about to disappear and are not reachable.
    let mut unreachable_packed: BTreeMap<ObjectId, i64> = BTreeMap::new();
    if !already_packed {
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
        let mut writer = PackWriter::new(format);
        for oid in &to_pack {
            let object = odb.read(oid)?;
            writer.write_object(&object)?;
        }
        let (_, summary) = writer.finish_to_path(&pack_dir)?;
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

    if already_packed {
        return finish_loose(repo, options, &reachable, &mut report);
    }

    // Unreachable objects young enough to survive are written back out as loose objects.
    for (oid, mtime) in &unreachable_packed {
        let expired = options.prune_before.is_some_and(|cutoff| *mtime <= cutoff);
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
    for oid in &loose_ids {
        if reachable.contains(oid) {
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
        for (oid, mtime) in ages {
            if mtime <= cutoff {
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
