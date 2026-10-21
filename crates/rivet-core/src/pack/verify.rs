//! Pack verification: the checks behind `verify-pack`.

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::ObjectKind;
use crate::pack::reader::PackFile;
use crate::pack::{EntryKind, crc32};

/// What verification learned about one packed object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedObject {
    /// The object's id, as the index records it.
    pub oid: ObjectId,
    /// The object's real kind (a delta reports the kind of its base).
    pub kind: ObjectKind,
    /// The entry's inflated size as the pack records it: the object's size, or for a delta the
    /// size of the delta data (which is what `git verify-pack -v` prints).
    pub size: u64,
    /// Bytes the entry occupies in the pack, header included.
    pub size_in_pack: u64,
    /// Where the entry starts in the pack.
    pub offset: u64,
    /// Length of the delta chain (0 for a whole object).
    pub depth: u32,
    /// The id of the direct delta base, for a delta.
    pub base: Option<ObjectId>,
}

/// Verify a pack against its index and return one record per object, ordered by offset.
///
/// Checks the trailing checksum, that the index and the pack agree on the object count and on
/// every offset, each entry's CRC-32, and that every object — deltas resolved — hashes to the id
/// the index gives it.
///
/// # Errors
///
/// [`Error::CorruptPack`] for any mismatch, [`Error::ThinPack`] when a delta base is not in the
/// pack, and the errors of inflating and delta application.
pub fn verify_pack(pack: &PackFile<'_>) -> Result<Vec<VerifiedObject>> {
    let corrupt = |reason: &str| Error::CorruptPack {
        reason: reason.to_string(),
    };
    let index = pack
        .index()
        .ok_or_else(|| corrupt("verify-pack needs an index"))?;
    pack.verify_checksum()?;
    if index.len() != pack.object_count() as usize {
        return Err(corrupt("index and pack disagree on the number of objects"));
    }

    let by_offset: HashMap<u64, crate::pack::PackIndexEntry> =
        index.entries().into_iter().map(|e| (e.offset, e)).collect();
    if by_offset.len() != index.len() {
        return Err(corrupt("index lists two objects at one offset"));
    }
    let by_oid: HashMap<ObjectId, u64> = by_offset.values().map(|e| (e.oid, e.offset)).collect();

    // Walk the pack in file order; the index must account for every entry exactly.
    let mut offsets = Vec::with_capacity(index.len());
    let mut next = 12u64;
    for item in pack.iter_entries() {
        let (entry, _) = item?;
        if entry.offset != next && !by_offset.contains_key(&entry.offset) {
            return Err(corrupt("an entry is missing from the index"));
        }
        offsets.push(entry.offset);
        let range = pack.entry_range(entry.offset)?;
        next = range.end as u64;
    }
    if offsets.len() != index.len() {
        return Err(corrupt("pack has fewer entries than the index"));
    }

    let mut out = Vec::with_capacity(offsets.len());
    for (position, offset) in offsets.iter().enumerate() {
        let indexed = by_offset
            .get(offset)
            .ok_or_else(|| corrupt("an entry is missing from the index"))?;
        let range = pack.entry_range(*offset)?;
        let raw = pack_bytes(pack, range.clone())?;
        if crc32(raw) != indexed.crc32 {
            return Err(corrupt("entry CRC does not match the index"));
        }

        let (kind, data) = pack
            .get_at_offset(*offset, None)
            .map_err(|error| match error {
                Error::MissingDeltaBase { oid, base } => Error::ThinPack { oid, base },
                other => other,
            })?;
        let object = crate::pack::wrap_object(kind, data);
        let actual = object.id(pack.format())?;
        if actual != indexed.oid {
            return Err(Error::HashMismatch {
                expected: indexed.oid,
                actual,
            });
        }

        // Follow the chain for depth and the direct base.
        let mut depth = 0u32;
        let mut base = None;
        let mut current = pack.entry_at(*offset)?;
        loop {
            let parent = match current.kind {
                EntryKind::OfsDelta => current.base_offset,
                EntryKind::RefDelta => current.base_oid.and_then(|oid| by_oid.get(&oid).copied()),
                _ => None,
            };
            let Some(parent) = parent else { break };
            depth += 1;
            if base.is_none() {
                base = by_offset.get(&parent).map(|e| e.oid);
            }
            if depth as usize > crate::pack::MAX_DELTA_DEPTH {
                return Err(Error::DeltaChainTooDeep {
                    oid: indexed.oid,
                    depth: crate::pack::MAX_DELTA_DEPTH,
                });
            }
            current = pack.entry_at(parent)?;
        }

        let size_in_pack = (range.end - range.start) as u64;
        let _ = position;
        out.push(VerifiedObject {
            oid: indexed.oid,
            kind: object.kind(),
            size: pack.entry_at(*offset)?.size,
            size_in_pack,
            offset: *offset,
            depth,
            base,
        });
    }
    out.sort_by_key(|o| o.offset);
    Ok(out)
}

fn pack_bytes<'a>(pack: &'a PackFile<'_>, range: std::ops::Range<usize>) -> Result<&'a [u8]> {
    pack.raw().get(range).ok_or_else(|| Error::CorruptPack {
        reason: "entry range is outside the pack".to_string(),
    })
}
