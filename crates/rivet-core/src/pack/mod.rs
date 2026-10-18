//! Phase 10 — packfiles: the container Git keeps almost every object in.
//!
//! A clone's object store is mostly packs. They are what a remote sends, what `git gc` builds and
//! what `git repack -ad` leaves behind, so the pack reader is the prerequisite for Phase 11 and
//! the hardest single piece of the plan (§8).
//!
//! # The files
//!
//! A pack is a base name plus two files that must agree:
//!
//! ```text
//! pack-<pack-checksum>.pack   the objects
//! pack-<pack-checksum>.idx    the v2 index: where each object is
//! ```
//!
//! The base name is the pack's own trailing checksum in hex, which is how a `.pack` and its
//! `.idx` are paired without trusting the filename: a reader must confirm that the `.idx`'s copy
//! of the pack checksum equals the one in the `.pack`'s trailer, or a stale `.idx` from a
//! half-finished `repack` will send every lookup to the wrong offset.
//!
//! # The pack layout
//!
//! ```text
//! "PACK"                 4 bytes, literal
//! version                4 bytes big-endian: 2, or 3 for the experimental format
//! object count           4 bytes big-endian
//! entry 0                see below
//! entry 1
//! …
//! trailer                the checksum of everything above, at the format's width
//! ```
//!
//! An entry is a header, optional base information, then a zlib stream:
//!
//! ```text
//! header   byte 0: [continuation][3-bit type][4-bit size]
//!          byte n: [continuation][7-bit size] shifted by 4, then 11, 18, …
//! OFS_DELTA   a variable-length negative distance to the base, back from this entry
//! REF_DELTA   the base's object id, at the repository's id width
//! zlib     the payload, either the object itself or a delta instruction stream
//! ```
//!
//! There is no length for the zlib stream and no offset table inside the `.pack`: an entry ends
//! where its zlib stream ends, which is why a sequential reader (`index-pack`) must inflate each
//! entry to find the next one, and why random access goes through the `.idx` instead.
//!
//! # Caveats that bite
//!
//! * **The entry header's size is the *inflated* size of that entry's own payload.** For a delta
//!   that is the length of the instruction stream, **not** the length of the object it
//!   reconstructs. A reader that reports it as the object's size produces plausible-looking
//!   `cat-file -s` output that is wrong for every deltified object.
//! * **The size varint's first group is 4 bits, later groups are 7.** Mixing it up with the
//!   7-bit-everywhere varint used for object sizes in loose headers is the classic off-by-eleven.
//! * **Every entry is compressed separately.** Deltas compress against another object's
//!   *content*, not against another zlib stream; nothing here chains decompression state.
//! * **Zlib is not raw deflate.** Loose objects and pack entries both use zlib framing (an RFC
//!   1950 header and an Adler-32 trailer). Raw deflate streams read as corruption, and `flate2`
//!   can produce either, so the choice is explicit in [`inflate_at`] and [`deflate`].
//! * **A pack is not self-describing about the hash width.** Neither the `.pack` header nor the
//!   `.idx` records whether ids are 20 or 32 bytes; it comes from
//!   `extensions.objectFormat`, exactly as it does for trees and the index. This is why every
//!   entry point here takes an [`ObjectFormat`].
//!
//! # Undeltified writes first
//!
//! Phase 10's write side starts with [`PackWriter`], which emits only undeltified entries. That
//! is fully valid and `git verify-pack` accepts it — it is simply larger than what
//! `git repack` would produce. Delta compression on write is a later step (plan §4 Phase 12) and
//! does not change any signature here: `write_object` would gain a "find a similar base" step,
//! not a new parameter.

use std::fmt;

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{Object, ObjectKind};

pub mod delta;
pub mod index;
pub mod reader;
pub mod writer;

pub use delta::{
    DeltaBase, DeltaHeader, apply_delta, decode_ofs_distance, parse_delta_header, resolve_delta,
};
pub use index::{OwnedPackIndex, PackIndex, PackIndexEntry, PackIndexIter};
pub use reader::{OwnedPackFile, PackFile, PackReader};
pub use writer::{PackWriteSummary, PackWriter};

/// Phase 10 — somewhere a delta base can be fetched from when it is not in the pack.
///
/// A `REF_DELTA` names its base by object id and says nothing about where that object is. In a
/// **thin** pack — what every fetch and push sends, by design — the base was deliberately not sent,
/// because the sender believed the receiver already had it. Resolving such a delta therefore
/// requires the rest of the object database, and this trait is the seam that keeps the pack code
/// from having to know about it: [`crate::pack::resolve_delta`] takes one of these, and
/// [`crate::odb::Odb`] is the implementation that can look anywhere.
///
/// # Caveats
///
/// * **Implementations must return the fully resolved object**, with its own delta chain already
///   applied — not the raw entry payload. A provider that returned an instruction stream would be
///   handing a delta to a delta, and [`crate::pack::apply_delta`] would fail on it in a way that
///   looks like corruption rather than like a wrong provider.
/// * **Implementations must bound re-entry.** Nothing stops a pack from naming bases that resolve
///   back to the object being resolved — `A` deltified against `B` in another pack, `B` against `A`
///   — and because the recursion crosses a trait object the depth counter inside a single
///   `resolve_delta` call does not see it. An implementation that recurses without tracking which
///   ids are already in flight will overflow the stack on a hostile pack; the required behaviour is
///   [`crate::error::Error::DeltaChainTooDeep`], not an abort (plan §5).
/// * **`Debug` is a supertrait** so that a pack reader holding one behind a reference can keep its
///   own `Debug` derive, matching [`crate::hash::ObjectHasher`].
/// * **This is not a general object store.** It answers exactly one question, because a general
///   store here would drag object-database concerns — caching policy, alternates, packs — into the
///   pack format's code.
pub trait BaseProvider: fmt::Debug {
    /// Phase 10 — fetch the object with this id, resolved, for use as a delta base.
    ///
    /// # Caveats
    ///
    /// The kind is returned alongside the bytes because a delta payload does not record what it
    /// reconstructs: a delta against a tree reconstructs a tree, and only the base knows that. A
    /// provider that guessed the kind from the bytes would be wrong for every object whose payload
    /// happens to parse as something else.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if the object is genuinely absent, which for a delta
    /// chain means the pack is thin against a repository that does not have the base either — the
    /// condition that surfaces to the user as [`crate::error::Error::MissingDeltaBase`], and the
    /// reason `index-pack --fix-thin` must be run before such a pack can be stored.
    fn base_object(&self, oid: &ObjectId) -> Result<(ObjectKind, Vec<u8>)>;
}

/// Phase 10 — the deepest delta chain Rivet will follow before reporting
/// [`Error::DeltaChainTooDeep`].
///
/// # Caveats
///
/// 50 is Git's `pack.depth` default, and matching it is what lets Rivet read anything `git
/// repack` produced at default settings. A repository configured with a deeper `pack.depth` (Git
/// caps it at 4095) can therefore contain a chain Rivet refuses — raising this to that cap is
/// safe, but it must stay bounded: `REF_DELTA` chains can be circular, and the guard is the only
/// thing standing between a hostile pack and a stack overflow.
pub const MAX_DELTA_DEPTH: usize = 50;

/// Phase 10 — what a pack entry's 3-bit type nibble says the entry is.
///
/// # Caveats
///
/// This is deliberately **not** [`ObjectKind`]. A pack entry can be a delta, which is a storage
/// detail rather than a fifth and sixth object type — Git has exactly four object kinds no matter
/// how an object is stored. [`EntryKind::object_kind`] returns `None` for a delta precisely so a
/// caller cannot hand a delta to code expecting a tag.
///
/// The nibble values (1–4 for the four kinds, 6 for `OFS_DELTA`, 7 for `REF_DELTA`) are part of
/// the on-disk format and cannot be reordered. `0` and `5` are unassigned: Git treats them as
/// corruption, and so must Rivet, rather than mapping them onto a kind "just in case".
///
/// `OFS_DELTA` and `REF_DELTA` store the *same* delta payload; only the way the base is named
/// differs. Both must be supported: `git repack` emits `OFS_DELTA` (it is smaller, because an
/// offset needs no 20 bytes), while a pack sent by `upload-pack` can use `REF_DELTA` for bases the
/// sender believes the receiver already has, which is what makes a pack thin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryKind {
    /// Pack type 1 — a commit, stored whole.
    Commit,
    /// Pack type 2 — a tree, stored whole.
    Tree,
    /// Pack type 3 — a blob, stored whole.
    Blob,
    /// Pack type 4 — an annotated tag, stored whole.
    Tag,
    /// Pack type 6 — a delta against an object **earlier in this same pack**, named by the
    /// distance back to it.
    OfsDelta,
    /// Pack type 7 — a delta against an object named by id, which need not be in this pack (and
    /// usually is not, in a thin pack).
    RefDelta,
}

impl EntryKind {
    /// Phase 10 — decode the type nibble from an entry header.
    ///
    /// # Caveats
    ///
    /// Takes the three bits already extracted (bits 4–6 of the entry's first byte), not the byte
    /// itself. Masking the wrong bits yields `Blob` or `Commit` for almost every real entry, so
    /// the mistake reads as a pack full of blobs rather than as an error.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptPack`] for the unassigned values `0` and `5`. These are not
    /// forward-compatibility land: a future pack format would bump the version field instead.
    pub fn from_nibble(nibble: u8) -> Result<Self> {
        todo!()
    }

    /// Phase 10 — the entry kind that stores an object of this kind whole.
    ///
    /// # Caveats
    ///
    /// Always one of the four undeltified kinds. Nothing in the public API needs the reverse
    /// direction for deltas — a caller never decides "this object should be stored as
    /// `OFS_DELTA`" without also having chosen the base and computed the offset, which is
    /// [`PackWriter`]'s business once delta compression exists.
    pub const fn of(kind: ObjectKind) -> Self {
        todo!()
    }

    /// Phase 10 — the object kind this entry encodes, or `None` if it is a delta.
    ///
    /// # Caveats
    ///
    /// `None` means "resolve the delta first", not "not an object". For a deltified entry the
    /// real kind is inherited from the base at the bottom of the chain — a delta against a tree
    /// is a tree — which is another reason a delta cannot be treated as its own object kind.
    pub const fn object_kind(self) -> Option<ObjectKind> {
        todo!()
    }

    /// Phase 10 — whether this entry is deltified, i.e. whether its payload is an instruction
    /// stream rather than object content.
    pub const fn is_delta(self) -> bool {
        todo!()
    }
}

/// Phase 10 — one entry header as it sits in a pack, decoded but not read.
///
/// This describes *where* an entry lives and what its payload looks like. It deliberately does
/// not hold the payload: the same entry is read by the reader, by the delta resolver and by
/// `index-pack`, and only the last of those wants the bytes for their own sake.
///
/// # Caveats
///
/// [`PackEntry::size`] is the length of **this entry's** inflated payload. For an undeltified
/// entry that is the object's length; for a delta it is the length of the instruction stream, and
/// the reconstructed object's length is inside the delta itself (see [`DeltaHeader`]). Reporting
/// the wrong one is invisible until a deltified object is read, which is to say: until a real
/// repository is opened.
///
/// `offset` points at the entry's first header byte — the one carrying the type nibble — which is
/// the value the `.idx` records and the value `git verify-pack -v` prints. It is *not* the offset
/// of the zlib stream, which is [`PackEntry::data_offset`].
///
/// [`PackEntry::base_offset`] is an absolute pack offset, not the encoded backward distance. The
/// decoding is already done, because the negative encoding is a property of the *entry* rather
/// than of anything a caller can use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackEntry {
    /// Absolute offset of the entry's first byte.
    pub offset: u64,
    /// What the entry's type nibble said.
    pub kind: EntryKind,
    /// Inflated length of this entry's own payload — see the caveat above.
    pub size: u64,
    /// For [`EntryKind::OfsDelta`], the absolute offset of the base. `None` for every other kind.
    pub base_offset: Option<u64>,
    /// For [`EntryKind::RefDelta`], the id of the base. `None` for every other kind.
    pub base_oid: Option<ObjectId>,
    /// Offset of the first byte of the entry's zlib stream, i.e. just past the header.
    pub data_offset: u64,
}

/// Phase 10 — what [`index_pack`] produced.
///
/// # Caveats
///
/// The entries appear **twice**: once here, in the order they were read out of the pack, and once
/// inside [`IndexPackResult::index`], sorted by object id, which is the order a `.idx` stores them
/// in. That duplication is deliberate — `verify-pack` and the thin-pack fixer need pack order,
/// which the sorted index does not preserve — but it is a second copy of every offset in a pack
/// that may hold a million objects. A caller that only wants the index should drop this struct as
/// soon as it has moved the index out.
#[derive(Debug)]
pub struct IndexPackResult {
    /// The pack index, ready to be serialised or queried.
    ///
    /// Owned rather than borrowed: `index-pack` discovers each entry's offset by walking the pack,
    /// so the index is built in memory and has no buffer to borrow from.
    pub index: OwnedPackIndex,
    /// The pack's own trailing checksum. This is also the pack's name: `pack-<hex>.pack`.
    pub pack_checksum: ObjectId,
    /// One row per entry, in pack order.
    pub entries: Vec<PackIndexEntry>,
    /// Whether any delta needed a base from outside the pack.
    ///
    /// True means the input was a **thin pack** and was only indexable because `bases` supplied
    /// the missing objects. Writing such a pack to disk unchanged leaves a pack that no other Git
    /// can read — it must go through [`fix_thin_pack`] first (plan §4 Phase 10).
    pub thin: bool,
}

/// Phase 10 — index a pack: read every entry, resolve every delta, and build the `.idx` v2 that
/// describes it.
///
/// This is the whole of `index-pack` except for the I/O around it, and it is what a fetch runs on
/// every pack it receives before any of those objects can be read.
///
/// # Caveats
///
/// * **Partial application leaves nothing behind.** The result is built in memory and returned
///   whole; the caller writes it. A pack that fails halfway must not leave a `.idx` that points
///   into an unverified `.pack`, because the next reader would trust it.
/// * **Every entry is verified against its declared size and its recomputed object id.** The id is
///   the only thing that binds an entry to its name, so an entry whose content hashes to something
///   else is corruption ([`crate::error::Error::HashMismatch`]) even though the zlib stream
///   inflated cleanly.
/// * **`index_pack` does not write a delta-free pack.** The result describes the input as it
///   arrived, deltas and all. Rerunning `index-pack` is not how a thin pack is fixed; that is
///   [`fix_thin_pack`].
/// * The pack's own trailing checksum is verified before anything else is trusted. A pack that
///   fails it is [`crate::error::Error::CorruptPack`] regardless of how well the entries decode.
///
/// # Errors
///
/// [`crate::error::Error::CorruptPack`] for a bad signature, an unsupported version, a truncated
/// entry, a bad zlib stream, a reserved type nibble, or a trailer checksum that does not match;
/// [`crate::error::Error::CorruptDelta`] and [`crate::error::Error::DeltaChainTooDeep`] for a
/// delta that does not apply; [`crate::error::Error::ThinPack`] when a `REF_DELTA` names a base
/// that is neither in the pack nor available through `bases`.
pub fn index_pack(
    pack: &[u8],
    format: ObjectFormat,
    bases: Option<&dyn BaseProvider>,
) -> Result<IndexPackResult> {
    todo!()
}

/// Phase 10 — complete a thin pack by appending the base objects it omitted, so that it can be
/// read on its own.
///
/// Fetch and push both receive thin packs by design (plan §4 Phase 10): the sender omits objects
/// it believes the receiver already has, and names them in `REF_DELTA` entries. Such a pack is
/// never written to `objects/pack` as it arrived — a pack whose bases live elsewhere is
/// unreadable by `git`, and worse, unreadable by Rivet the moment the object database changes.
///
/// # Caveats
///
/// * **The appended bytes change the pack's checksum**, and therefore its name, and therefore the
///   `.idx` filenames. The caller must index and write the *returned* bytes, not the input's.
/// * **The base objects do not have to be inserted anywhere clever.** Git's `--fix-thin` appends
///   them after the last original entry; `REF_DELTA` entries keep naming their bases by id, which
///   now resolve because the `.idx` built over the completed pack contains them. Nothing already
///   in the pack is rewritten, so no offset moves.
/// * An input that is **not** thin is not an error: every base already resolves inside the pack
///   and the input is returned unchanged (byte-for-byte, so the checksum is unchanged too).
/// * The bases are appended **undeltified**. Deltifying them against each other would be smaller
///   but is not what the format requires, and a pack whose appended bases are themselves deltas
///   against objects that are not present would be thin again.
///
/// # Errors
///
/// The same failures as [`index_pack`], since it must fully index the input to discover which
/// bases are missing; plus [`crate::error::Error::MissingDeltaBase`] when `bases` cannot supply
/// one of the bases the pack names.
pub fn fix_thin_pack(
    pack: &[u8],
    format: ObjectFormat,
    bases: &dyn BaseProvider,
) -> Result<Vec<u8>> {
    todo!()
}

/// Phase 10 — wrap an owned payload as the [`Object`] variant its kind names.
///
/// # Caveats
///
/// A `pub(crate)` convenience for the reader and the delta resolver, which both hold a
/// `(kind, Vec<u8>)` pair that is really one object. It is not public because it cannot check
/// anything: the kind is taken on trust, so a caller outside the crate could build an
/// `Object::Commit` around a blob's bytes. Round-tripping through
/// [`crate::object::AnyObject::parse`] is the checked path.
pub(crate) fn wrap_object(kind: ObjectKind, data: Vec<u8>) -> Object {
    match kind {
        ObjectKind::Blob => Object::Blob(data),
        ObjectKind::Tree => Object::Tree(data),
        ObjectKind::Commit => Object::Commit(data),
        ObjectKind::Tag => Object::Tag(data),
    }
}

/// Phase 10 — read a big-endian `u32` from `data` at `at`.
///
/// # Caveats
///
/// Every integer in a pack and a `.idx` v2 is big-endian, including the entry headers' size
/// groups. This is also the only shared byte reader in the module: leaving it inline at each call
/// site is how a parser ends up with one little-endian field that only shows up on a pack whose
/// object count has a set byte in the "wrong" position.
///
/// # Errors
///
/// [`crate::error::Error::CorruptPack`] if fewer than four bytes remain. Callers that are reading
/// a `.idx` report the same failure as [`crate::error::Error::CorruptPackIndex`] instead, since
/// the bytes came from the other file.
pub(crate) fn read_u32_be(data: &[u8], at: usize) -> Result<u32> {
    todo!()
}

/// Phase 10 — read a big-endian `u64` from `data` at `at`, for the `.idx` 64-bit offset table.
///
/// # Errors
///
/// [`crate::error::Error::CorruptPackIndex`] if fewer than eight bytes remain.
pub(crate) fn read_u64_be(data: &[u8], at: usize) -> Result<u64> {
    todo!()
}

/// Phase 10 — inflate one zlib stream starting at `at`, returning the bytes and how many input
/// bytes the stream occupied.
///
/// The consumed length is the only way to find the next entry in a pack read sequentially, since
/// the format records no compressed length anywhere.
///
/// # Caveats
///
/// * **This is zlib, not raw deflate** — an RFC 1950 wrapper with a header and an Adler-32
///   trailer. A stream produced with `flate2`'s raw-deflate mode inflates to nothing useful and
///   must be rejected.
/// * **Stops at the end of the first stream.** Trailing bytes after the stream belong to the next
///   entry (or to the pack trailer) and must not be consumed, so the decoder cannot be asked to
///   "inflate the rest".
/// * **The caller checks the length, not this function.** A loose object knows its expected size
///   from its header and a pack entry from its entry header; neither expectation can be passed in
///   here without making the pack reader's "size is discovered after inflation" case awkward.
/// * A corrupt stream fails here with [`crate::error::Error::CorruptPack`], which the loose
///   object store reports as [`crate::error::Error::CorruptObject`] instead — the bytes came from
///   a different place and the caller distinguishes them (§5).
///
/// # Errors
///
/// [`crate::error::Error::CorruptPack`] if `at` is past the end, the stream is truncated, or the
/// zlib framing is invalid.
pub(crate) fn inflate_at(data: &[u8], at: usize) -> Result<(Vec<u8>, usize)> {
    todo!()
}

/// Phase 10 — deflate with zlib framing, the inverse of [`inflate_at`].
///
/// # Caveats
///
/// **The compression level is not part of the format**, but it is visible on disk and therefore
/// visible to the differential tests. Git uses zlib's default level (6) and Rivet must match it
/// when writing, or a repository that Rivet wrote will differ byte-for-byte from one Git wrote for
/// the same objects — a diff that is "correct" and useless to debug. Compressing with level 0
/// (stored) is valid and tempting for tests; it produces a legal pack that is roughly the size of
/// the input.
///
/// # Errors
///
/// [`crate::error::Error::CorruptPack`] only as a wrapper for a compressor failure; with a valid
/// backend this is not reachable.
pub(crate) fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    todo!()
}

/// Phase 10 — CRC-32 of a pack entry's on-disk bytes, as the `.idx` v2 records it.
///
/// # Caveats
///
/// * The range is the **compressed** entry: from the first header byte through the last byte of
///   the zlib stream, inclusive. Hashing the inflated payload instead produces a `.idx` that Git
///   reads happily and `git verify-pack` rejects, which is exactly the failure mode that makes
///   `verify-pack` worth running on everything Rivet writes.
/// * The last entry's range ends at the pack trailer, not at the end of the file.
/// * This is the same CRC-32 as zlib's (the IEEE polynomial), and `flate2` already exposes it —
///   do not hand-roll a table, and do not substitute a different 32-bit check.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    todo!()
}
