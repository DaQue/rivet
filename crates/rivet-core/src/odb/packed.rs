//! Phase 10 — the packed half of the object database: one `.pack` and its `.idx`.
//!
//! This is the store that makes a `git gc`'d repository readable. It pairs a [`crate::pack::PackFile`] with the
//! [`crate::pack::PackIndex`] beside it so that an object id can be turned into an offset, and it keeps the
//! resolved delta bases that make a chain of deltas affordable to walk.
//!
//! # Caveats that bite
//!
//! * **A pack is immutable.** Once the trailing checksum exists, the file's name is fixed and no
//!   entry can be added or rewritten without producing a different pack. Nothing in this module
//!   writes: [`crate::pack::PackWriter`] builds a whole pack, and a new one is registered by
//!   [`crate::odb::Odb::add_pack`]. That is why there is no `insert` here.
//! * **The pair must belong together.** The `.idx` records the `.pack`'s checksum precisely so a
//!   stale index — the leftovers of an interrupted `repack`, or a `.idx` copied beside the wrong
//!   pack — is detectable. Registration verifies it once; nothing re-checks it per read.
//! * **The store holds the pack's bytes.** A caller that opened a repository with a gigabyte of
//!   packs holds a gigabyte. mmap is the Phase 12 fix and does not change this signature.
//! * **A lookup miss is not an error.** An object absent from this pack may be loose or in another
//!   pack, and deciding that is [`crate::odb::Odb`]'s job. Returning
//!   [`crate::error::Error::ObjectNotFound`] from here for a miss would force the caller to match
//!   on it in the ordinary "try the next store" path, which is a control-flow-through-errors shape
//!   that gets the corrupt cases wrong.
//! * **Delta chains cross the pack boundary.** A `REF_DELTA` names its base by id, and in a thin
//!   pack that base is not here at all. This store cannot satisfy that on its own — it is why reads
//!   take a [`BaseProvider`] — and reporting a missing base as corruption rather than as
//!   thin-ness is the mistake that turns a completed fetch into a failed one.
//!
//! # Why a cache, and why only for bases
//!
//! Resolving a deltified object re-inflates every base down its chain, and a tree walk reads many
//! objects that share long chains — so the same base is decompressed over and over. Caching *whole
//! objects* would be the wrong trade for a CLI: each command reads a handful of unrelated objects,
//! so a content cache pays memory for hits that never come. Bases are different: they are provably
//! reused within a single walk, and the chain below them is the expensive part.
//!
//! The cache is keyed by pack checksum and offset rather than by object id, because that is what a
//! delta actually names — an `OFS_DELTA` never mentions an id at all, and looking one up would mean
//! an index search per link.

use crate::error::Error;
use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{Object, ObjectKind};
use crate::pack::{BaseProvider, OwnedPackFile, OwnedPackIndex};

/// Phase 10 — one pack and its index, as a searchable store.
///
/// # Caveats
///
/// * **Construction verifies the pair once** — the pack's magic, version and trailer, and the
///   index's structure and its copy of the pack checksum. It does not verify the index's *offsets*,
///   which would mean walking the pack; a wrong offset is caught when an entry is read, as
///   [`crate::error::Error::CorruptPack`] or, less helpfully, as a hash mismatch.
/// * **Reads take a `&self` and share nothing mutable.** The delta cache lives in
///   [`crate::odb::Odb`], above this type, because it must be keyed across packs and outlive a
///   single pack's usefulness. A `PackedObjectStore` used alone on a deltified pack therefore
///   re-inflates bases on every read, which is correct and slow.
/// * **Not `Eq`.** Two stores are equal when they describe the same pack, which is a question
///   about a checksum rather than about bytes held in memory; compare
///   [`PackedObjectStore::checksum`] where it matters.
///
/// # Panics
///
/// None. Every failure is an [`crate::error::Error`] (plan §5).
#[derive(Clone, Debug)]
pub struct PackedObjectStore {
    /// The pack and its index, owning both buffers.
    pack: OwnedPackFile,
    /// The parsed index. Kept alongside the raw bytes so a lookup does not re-parse per call.
    index: OwnedPackIndex,
    /// The repository's id width, taken from `extensions.objectFormat` when the repository was
    /// opened. Not recorded in either file.
    format: ObjectFormat,
}

impl PackedObjectStore {
    /// Phase 10 — register a pack and its index, verifying they belong together.
    ///
    /// # Caveats
    ///
    /// * **The index is required.** A pack without one cannot be searched by id at all, so a store
    ///   built without it would be a handle whose every lookup misses. Letting that state exist
    ///   would make "not found" ambiguous between "absent" and "unindexed", and the caller that
    ///   has an unindexed pack should run [`crate::pack::index_pack`] first.
    /// * **The pack checksum recorded in the index must equal the pack's trailer.** This is the
    ///   only cheap check that the two files describe each other, and it is what catches the stale
    ///   `.idx` left by an interrupted `repack` — a state that is otherwise indistinguishable from
    ///   a valid pair, and whose reads go to offsets that exist but hold different objects.
    /// * The pack's own checksum is **not** recomputed here. Hashing the whole file at open would
    ///   make every command linear in the repository's size; the entry-level id checks catch
    ///   corruption in practice, and whole-file verification is `verify-pack`'s explicit job.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`] for a bad magic, a version other than 2 or 3, a file
    /// too short to hold a header and trailer, or a trailer that disagrees with the index's
    /// recorded checksum; [`crate::error::Error::UnsupportedPackVersion`] for an unreadable
    /// version; [`crate::error::Error::CorruptPackIndex`] and
    /// [`crate::error::Error::UnsupportedPackIndexVersion`] from parsing the index.
    pub fn open(data: Vec<u8>, index: Vec<u8>, format: ObjectFormat) -> Result<Self> {
        let file = crate::pack::PackFile::open(&data, None, format)?;
        let index = OwnedPackIndex::parse(index, format)?;
        {
            let parsed = index.as_index();
            if parsed.pack_checksum() != file.checksum() {
                return Err(Error::CorruptPack {
                    reason: "index names a different pack checksum".to_string(),
                });
            }
            if parsed.len() != file.object_count() as usize {
                return Err(Error::CorruptPack {
                    reason: "index entry count disagrees with the pack header".to_string(),
                });
            }
        }
        Ok(Self {
            pack: OwnedPackFile::new(data, None, format),
            index,
            format,
        })
    }

    /// Phase 10 — how many objects the pack holds.
    ///
    /// # Caveats
    ///
    /// Read from the index's fanout table, which is the entry count as it was written. The pack
    /// header carries its own copy of the count and the two are compared at
    /// [`PackedObjectStore::open`]; a mismatch means the files do not describe each other.
    pub fn len(&self) -> usize {
        self.index.as_index().len()
    }

    /// Phase 10 — whether the pack holds no objects.
    ///
    /// # Caveats
    ///
    /// Legal, and Git writes such a pack when a push adds no objects the receiver lacks. It is not
    /// a reason to refuse the pack, and a caller that treats it as one will fail on a legitimate
    /// exchange.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Phase 10 — the repository's id width, as it was supplied at open.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// Phase 10 — the pack's checksum, which is also its name: `pack-<hex>.pack`.
    ///
    /// # Caveats
    ///
    /// Read from the trailer (and checked against the index's copy at open), not recomputed. Its
    /// main use is naming: a caller that has to write or find the `.idx` beside a `.pack` derives
    /// the stem from here rather than from a path it happened to be given, which is what lets a
    /// pack be recognised across a rename or a move into a different object directory.
    ///
    /// It is also the cache key's first half in [`crate::odb::Odb`]: offsets are only meaningful
    /// within one pack, so the checksum is what keeps two packs' offset spaces apart.
    pub fn checksum(&self) -> ObjectId {
        // The index's copy was verified equal to the pack trailer at open; reading it back avoids
        // holding a second borrow of the pack buffer for a value that cannot change.
        self.index.as_index().pack_checksum()
    }

    /// Phase 10 — the parsed index, for callers that need the tables themselves.
    ///
    /// # Caveats
    ///
    /// Exposed for `verify-pack`, which reports per-entry offsets and CRCs, and for `fsck`, which
    /// needs to confirm that every entry in the pack is described. Ordinary object reads should use
    /// [`PackedObjectStore::read`], which goes through the same index and additionally resolves
    /// deltas and verifies hashes — reading an entry by offset via the index skips both.
    pub fn index(&self) -> &OwnedPackIndex {
        &self.index
    }

    /// Phase 10 — whether this pack holds the object, without reading it.
    ///
    /// # Caveats
    ///
    /// * **A pure index query.** It does not touch the pack bytes, so a pack whose file has been
    ///   truncated underneath answers `true` for objects that can no longer be read. Existence is
    ///   the index's claim; only a read tests the pack.
    /// * **False is not an error and not a verdict.** The object may be loose or in a sibling pack;
    ///   this says only that *this* pack does not describe it.
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.index.as_index().lookup(oid).is_some()
    }

    /// Phase 10 — read one object, resolving its delta chain and verifying its hash.
    ///
    /// # Caveats
    ///
    /// * **The returned [`Object`] owns its bytes**, which for an undeltified entry is a copy of
    ///   nothing — the bytes are decompressed into a fresh buffer either way, so the ownership costs
    ///   no extra copy. A caller that only wants to parse should let the [`Object`] drop afterwards.
    /// * **The id is verified before returning.** This is the only point at which a deltified
    ///   object's correctness can be established: the bytes that came out of the pack are an
    ///   instruction stream that has no relationship of its own to the id, so a chain that resolved
    ///   against the wrong base produces a plausible-looking object until it is rehashed.
    /// * **`bases` supplies what the pack does not contain.** A `REF_DELTA` in a thin pack names an
    ///   object that was never sent, and the only correct answer is to ask the object database.
    ///   Passing `None` makes such a base
    ///   [`crate::error::Error::MissingDeltaBase`] — correct for a pack that is known to be
    ///   self-contained, and silently wrong for one that is not, which is why the caller that
    ///   received a pack over the network must pass `Some`.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if the index does not describe this id; the
    /// pack-level errors of [`PackedObjectStore::read_at`];
    /// [`crate::error::Error::MissingDeltaBase`] when a base is neither in the pack nor available
    /// through `bases`; and [`crate::error::Error::HashMismatch`] when the resolved content does
    /// not hash to `oid`.
    pub fn read(&self, oid: &ObjectId, bases: Option<&dyn BaseProvider>) -> Result<Object> {
        let entry = self
            .index
            .as_index()
            .lookup(oid)
            .ok_or(Error::ObjectNotFound { oid: *oid })?;
        let (kind, data) = self.read_at(entry.offset, bases)?;
        let obj = match kind {
            ObjectKind::Blob => Object::Blob(data),
            ObjectKind::Tree => Object::Tree(data),
            ObjectKind::Commit => Object::Commit(data),
            ObjectKind::Tag => Object::Tag(data),
        };
        let id = obj.id(self.format)?;
        if id != *oid {
            return Err(Error::HashMismatch {
                expected: *oid,
                actual: id,
            });
        }
        Ok(obj)
    }

    /// Phase 10 — read the object at a specific entry offset, without a hash check.
    ///
    /// The offset-addressed read that backs delta resolution: every `OFS_DELTA` in a chain is
    /// reached this way, and so is a `REF_DELTA` whose base is in this same pack.
    ///
    /// # Caveats
    ///
    /// * **No hash check is possible.** The caller supplied an offset, and the pack records no id
    ///   for it, so there is nothing to compare against. A caller using this directly has taken on
    ///   the verification itself; [`PackedObjectStore::read`] is the checked path.
    /// * **The offset is trusted to be an entry boundary.** One that lands mid-entry is caught when
    ///   the header decodes or the zlib stream fails, and the resulting error names corruption
    ///   rather than the offset, so a caller must not read a failure here as proof that the offset
    ///   was wrong.
    /// * **The kind comes back with the bytes** because a delta's payload does not record what it
    ///   reconstructs — the kind is inherited from the base at the bottom of the chain, and this is
    ///   where that inheritance happens.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`] for a bad entry header, a bad zlib stream or an offset
    /// outside the pack; [`crate::error::Error::CorruptDelta`] for a delta that does not apply;
    /// [`crate::error::Error::DeltaChainTooDeep`] at the depth limit; and
    /// [`crate::error::Error::MissingDeltaBase`] for an unresolvable base.
    pub fn read_at(
        &self,
        offset: u64,
        bases: Option<&dyn BaseProvider>,
    ) -> Result<(ObjectKind, Vec<u8>)> {
        self.pack.pack()?.get_at_offset(offset, bases)
    }

    /// Pack offset to object id, for resolving `OFS_DELTA` bases.
    pub fn oids_by_offset(&self) -> std::collections::HashMap<u64, ObjectId> {
        let index = self.index.as_index();
        let mut map = std::collections::HashMap::with_capacity(index.len());
        for i in 0..index.len() {
            if let Some(entry) = index.entry(i) {
                map.insert(entry.offset, entry.oid);
            }
        }
        map
    }

    /// If `oid` is stored here as a delta, its base object and the delta's instruction bytes.
    ///
    /// `by_offset` is [`PackedObjectStore::oids_by_offset`], passed in so a caller looking at many
    /// entries builds it once. `None` for an undeltified entry.
    ///
    /// # Errors
    ///
    /// The pack-level errors of reading the entry.
    pub fn stored_delta(
        &self,
        oid: &ObjectId,
        by_offset: &std::collections::HashMap<u64, ObjectId>,
    ) -> Result<Option<(ObjectId, Vec<u8>)>> {
        let Some(found) = self.index.as_index().lookup(oid) else {
            return Ok(None);
        };
        let pack = self.pack.pack()?;
        let entry = pack.entry_at(found.offset)?;
        let base = match (entry.kind, entry.base_offset, entry.base_oid) {
            (crate::pack::EntryKind::OfsDelta, Some(offset), _) => by_offset.get(&offset).copied(),
            (crate::pack::EntryKind::RefDelta, _, Some(base)) => Some(base),
            _ => None,
        };
        let Some(base) = base else {
            return Ok(None);
        };
        let (delta, _) = crate::pack::inflate_at(pack.raw(), entry.data_offset as usize)?;
        Ok(Some((base, delta)))
    }

    /// Phase 10 — every object id this pack contains, in id order.
    ///
    /// # Caveats
    ///
    /// Allocates one id per object, so a large pack costs tens of megabytes here. `fsck` needs it —
    /// it has to confirm that everything the pack claims is readable — and `gc` needs it to decide
    /// which loose objects a pack has already superseded. There is no lazy form, because both
    /// callers end up holding the set anyway.
    ///
    /// The order is the index's, which is sorted by id and is therefore **not** pack order. A
    /// caller that wants to stream the pack must use [`crate::pack::PackFile::iter_entries`].
    ///
    /// # Errors
    ///
    /// None from the tables, which were validated at open; the signature is [`Result`] so that a
    /// later implementation that re-validates the pair does not change every call site.
    pub fn oids(&self) -> Result<Vec<ObjectId>> {
        let index = self.index.as_index();
        let mut ids = Vec::with_capacity(index.len());
        for i in 0..index.len() {
            if let Some(entry) = index.entry(i) {
                ids.push(entry.oid);
            }
        }
        Ok(ids)
    }

    /// Phase 10 — confirm the pack's trailer by hashing its contents.
    ///
    /// # Caveats
    ///
    /// Linear in the pack's size and reads every byte, so this is `verify-pack` and `fsck` work and
    /// never something a normal read pays for. It is, however, the only check that covers bytes no
    /// entry claims — corruption inside a zlib stream that still inflates is caught per object, but
    /// a whole pack that was truncated and rehashed is only caught against its recorded checksum.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`] if the recomputed checksum differs from the trailer,
    /// and [`crate::error::Error::Io`] from the hasher.
    pub fn verify_checksum(&self) -> Result<()> {
        self.pack.pack()?.verify_checksum()
    }
}

/// Phase 10 — a bounded cache of resolved delta bases, shared across the packs of one database.
///
/// # Caveats
///
/// * **Keyed by pack checksum and offset, not by object id.** A delta names its base positionally —
///   an `OFS_DELTA` never mentions an id — so keying by id would require an index search per chain
///   link just to look the cache up. The checksum half of the key is what keeps two packs'
///   identical offsets apart; without it a cache would hand back an unrelated pack's bytes, which
///   would resolve to a plausible object and fail only at the final hash check.
/// * **Values are cloned on the way out**, because the cache is reached through a `RefCell` and a
///   borrow cannot escape it. That is a copy per hit, which is still far cheaper than re-inflating
///   a chain — the chain below a base is the expensive part, not the base's bytes.
/// * **Bounded and evicted arbitrarily.** The capacity is a ceiling in entries, not bytes, so a
///   cache full of large trees costs more than one full of small blobs. Eviction order is not
///   specified because no caller may depend on a hit: the cache is a pure optimisation, and a
///   correct implementation works identically with it disabled.
/// * **Contents are not verified.** The cache stores bytes that were already hash-checked by the
///   read that filled it; it must never be populated from a path that skipped that check, or it
///   becomes the one way a corrupt object reaches a caller without being noticed.
/// * **`pub(crate)`** — it is an implementation detail of [`crate::odb::Odb`], not part of the
///   crate's surface. Documented anyway, because its key design is the kind of thing a later change
///   breaks silently.
#[derive(Debug)]
pub(crate) struct DeltaBaseCache {
    /// Entries in insertion order, oldest first. A `Vec` rather than a map because the capacity is
    /// small and the access pattern is a chain walk, not a random lookup.
    entries: Vec<DeltaBaseEntry>,
    /// Maximum number of entries retained.
    capacity: usize,
}

/// Phase 10 — one cached delta base.
///
/// # Caveats
///
/// Holds the **resolved** bytes — the base after its own delta chain was applied — not the raw
/// entry payload. Caching the unresolved payload would save the inflate but not the chain walk, and
/// the chain walk is the part that is quadratic in the number of readers.
#[derive(Clone, Debug)]
pub(crate) struct DeltaBaseEntry {
    /// The pack the base came from, so two packs' offset spaces do not collide.
    pub(crate) pack: ObjectId,
    /// The base's entry offset within that pack.
    pub(crate) offset: u64,
    /// The base's object kind, inherited along the chain.
    pub(crate) kind: ObjectKind,
    /// The base's fully resolved content.
    pub(crate) data: Vec<u8>,
}

impl DeltaBaseCache {
    /// Phase 10 — a cache holding at most `capacity` resolved bases.
    ///
    /// # Caveats
    ///
    /// A capacity of zero is legal and makes every `get` a miss — the correct way to disable the
    /// cache, and preferable to an `Option<DeltaBaseCache>` at the call site because it keeps one
    /// code path. The capacity is a count of entries rather than bytes: sizing it by memory would
    /// require knowing each entry's size, which is only known after the miss it was meant to
    /// prevent.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
        }
    }

    /// Phase 10 — look up a base, returning an owned copy on a hit.
    ///
    /// # Caveats
    ///
    /// The copy is forced by the `RefCell` the cache lives in: a borrow of an entry cannot outlive
    /// the call, and returning a reference would require the caller to hold the borrow guard across
    /// the operation that might itself need to insert — which would panic at runtime on a
    /// reentrant `borrow_mut`. Cloning keeps the lock discipline trivial at the cost of a
    /// memcpy per hit.
    pub(crate) fn get(&self, pack: &ObjectId, offset: u64) -> Option<(ObjectKind, Vec<u8>)> {
        self.entries
            .iter()
            .find(|e| e.pack == *pack && e.offset == offset)
            .map(|e| (e.kind, e.data.clone()))
    }

    /// Phase 10 — insert a resolved base, evicting if full.
    ///
    /// # Caveats
    ///
    /// The caller must have already verified the bytes against their object id — see the type-level
    /// caveat. An existing entry for the same key is replaced rather than duplicated, because the
    /// same base can be resolved twice under different chain walks and keeping both would halve the
    /// effective capacity.
    pub(crate) fn insert(&mut self, pack: ObjectId, offset: u64, kind: ObjectKind, data: Vec<u8>) {
        // Replace an existing entry with the same key rather than duplicating it.
        let pos = self
            .entries
            .iter()
            .position(|e| e.pack == pack && e.offset == offset);
        if let Some(pos) = pos {
            self.entries[pos] = DeltaBaseEntry {
                pack,
                offset,
                kind,
                data,
            };
        } else {
            self.entries.push(DeltaBaseEntry {
                pack,
                offset,
                kind,
                data,
            });
            // Evict the oldest entries until back within capacity.
            while self.entries.len() > self.capacity {
                self.entries.remove(0);
            }
        }
    }
}
