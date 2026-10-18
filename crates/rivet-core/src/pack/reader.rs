//! Phase 10 — reading a packfile: the header, the sequential walk, and delivery through the index.
//!
//! There are two ways to read a pack and they are used for different things:
//!
//! * **Sequentially**, entry by entry from the start, which is the only way to discover the entry
//!   boundaries because the format stores no length for a zlib stream and no table of contents.
//!   This is [`PackReader`], and it is what `index-pack` and `verify-pack` run.
//! * **By lookup**, jumping to an offset the `.idx` supplied. This is [`PackFile::get`], and it is
//!   what every normal object read does.
//!
//! Both are needed. A repository opened from a `git gc`'d clone has packs and no loose objects, so
//! [`PackFile::get`] is on the path of nearly every command; a fetch has a pack and no `.idx` yet,
//! so [`PackReader`] is on the path of every clone.
//!
//! # Caveats that bite
//!
//! * **The version is 2 or 3.** Both are read; v3 differs only in the compression of `OFS_DELTA`
//!   bases in packs that Git never produced publicly, and Git itself stopped emitting it. Any
//!   other version is [`crate::error::Error::UnsupportedPackVersion`] — the count and everything
//!   after it would be misread otherwise.
//! * **The object count is a `u32` and is not a length.** Nothing in the header says how long the
//!   file is, so the count is a claim; the walk must verify it against the trailer rather than
//!   trusting it to bound the loop.
//! * **The `PACK` magic is checked before the version**, and a file that is not a pack at all gets
//!   the same error as one with a bad version. That is deliberate: distinguishing them would leak
//!   "this file exists and is not a pack" into a code path that is usually a race against a
//!   concurrent `repack`.
//! * **The trailer is the pack's name.** `pack-<hex>.pack` is derived from those bytes, so a
//!   reader that has the checksum does not need the path to know which `.idx` belongs beside it.
//! * **A `.idx` may be absent, and that is legal.** A pack received from a fetch has no index until
//!   Rivet writes one, and a pack whose `.idx` failed to parse is still readable sequentially.
//!   [`PackFile::open`] must therefore not require one (see [`crate::odb::Odb`] for how a pack
//!   without an index is still searched).
//! * **Nothing here verifies the checksum by default.** Hashing a 2 GB pack on every open is not
//!   acceptable; the entry-level id checks in [`crate::odb`] are what catch corruption in practice,
//!   and whole-pack verification is an explicit `verify-pack` operation.

use std::ops::Range;
use std::path::Path;

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{Object, ObjectKind};
use crate::pack::{BaseProvider, PackEntry, PackIndex};

/// Phase 10 — one packfile, opened over a borrowed buffer.
///
/// This is the read handle for a single `.pack`: it answers "which objects are in here" from the
/// [`PackIndex`] beside it and "give me the bytes of this object" from the pack itself, resolving
/// `OFS_DELTA` and `REF_DELTA` chains on the way.
///
/// # Caveats
///
/// * **The `.idx` borrows the same buffer lifetime as the pack**, which is why both share one
///   lifetime parameter. A caller that mmaps the pair keeps the mappings alive for as long as the
///   `PackFile` exists; a caller that reads the files into `Vec`s must keep both `Vec`s alive and
///   must not let them be reallocated.
/// * **The index is optional.** A freshly received pack has none. [`PackFile::get`] over such a
///   pack is not merely slow, it is impossible — the offsets are unknown until
///   [`crate::pack::index_pack`] has run — and the honest answer is
///   [`crate::error::Error::ObjectNotFound`] rather than a scan. [`PackFile::iter_entries`] still
///   works.
/// * **A `REF_DELTA` may point outside this pack.** In a thin pack the base was never sent, and
///   resolution needs the rest of the object database. That is why the resolver lives in
///   [`crate::odb::Odb`] rather than here: this type cannot see loose objects or other packs, and
///   must report [`crate::error::Error::ThinPack`] rather than pretend the base is missing.
/// * **Offsets are absolute and unvalidated until read.** An index may claim an offset that is past
///   the end of the pack, or into the middle of another entry; that is caught when the entry header
///   is decoded, as [`crate::error::Error::CorruptPack`], and not from the index.
/// * **Delta chains cross entries, so a `get` is not a single entry read.** Resolving an object
///   may inflate a dozen entries, all within this pack. The cache in [`crate::odb::Odb`] is what
///   keeps a walk over a deltified tree from re-inflating the same base repeatedly; this type does
///   no caching of its own, deliberately, because its lifetime is a single call site's.
#[derive(Clone, Debug)]
pub struct PackFile<'a> {
    /// The `.pack` bytes. Borrowed, not owned — see the caveats on lifetimes.
    data: &'a [u8],
    /// The parsed `.idx`, when one was found. `None` for a pack that has not been indexed yet.
    index: Option<PackIndex<'a>>,
    /// The repository's id width. Not recorded in the pack; supplied by the caller from
    /// `extensions.objectFormat`.
    format: ObjectFormat,
    /// The pack's own version from its header, for `verify-pack` and error messages.
    version: u32,
}

impl<'a> PackFile<'a> {
    /// Phase 10 — open a pack over a borrowed buffer, with an optional index.
    ///
    /// # Caveats
    ///
    /// * **`data` must include the trailer.** The pack's checksum is its last `format.len()` bytes
    ///   and is checked against the index's copy of it; a buffer that stops after the last entry
    ///   looks like a truncated pack.
    /// * **`index` is not verified against `data` here beyond the shared checksum.** Confirming
    ///   that every offset in the index lands on a valid entry would mean walking the whole pack,
    ///   which defeats the point of having an index.
    /// * Passing an index parsed with a different `format` than this call's is not detectable and
    ///   will misread every id. The format is a property of the repository, and every caller in
    ///   this crate gets it from the same place.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`] for a bad `PACK` magic, a buffer shorter than a header
    /// and trailer, or a trailer that does not match the index's copy of the pack checksum;
    /// [`crate::error::Error::UnsupportedPackVersion`] for a version other than 2 or 3.
    pub fn open(
        data: &'a [u8],
        index: Option<PackIndex<'a>>,
        format: ObjectFormat,
    ) -> Result<Self> {
        todo!()
    }

    /// Phase 10 — open a pack from `.pack` and `.idx` files on disk.
    ///
    /// # Caveats
    ///
    /// * **Reads both files into memory.** Fine for the sizes Rivet targets, and it keeps the
    ///   borrow story honest until mmap arrives in Phase 12; a caller that wants mmap maps the
    ///   files itself and calls [`PackFile::open`].
    /// * **A missing `.idx` is only an error if the caller needs one.** This constructor loads
    ///   whichever half is there and reports the failure of the `.idx` read as absent rather than
    ///   fatal — a pack whose index is still being written by another process is readable, and
    ///   saying so is better than a hard error on a transient state.
    /// * A `.idx` that exists but is corrupt **is** fatal, rather than silently degraded to "no
    ///   index": the pack it describes is very likely corrupt too, and reading around it would
    ///   produce objects that `git fsck` rejects.
    ///
    /// # Errors
    ///
    /// Whatever [`PackFile::open`] raises, plus [`crate::error::Error::IoAtPath`] for the `.pack`
    /// itself.
    pub fn open_path(pack: &Path, format: ObjectFormat) -> Result<OwnedPackFile> {
        todo!()
    }

    /// Phase 10 — the pack's version from its header.
    ///
    /// # Caveats
    ///
    /// 2 or 3. This is not the `.idx` version, which is separately and always 2 for what Rivet
    /// reads — a pack v3 with an index v2 is the normal pairing, and confusing the two produces
    /// wrong error messages rather than wrong data.
    pub fn version(&self) -> u32 {
        todo!()
    }

    /// Phase 10 — the repository's id width, as it was passed to [`PackFile::open`].
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// Phase 10 — the pack's checksum, read from its trailer.
    ///
    /// # Caveats
    ///
    /// The trailer, not a recomputation — this is fast and equals the filename's hex. To actually
    /// *verify* it, hash the bytes up to the trailer and compare; that is what
    /// [`PackFile::verify_checksum`] is for, and it is linear in the pack's size.
    pub fn checksum(&self) -> ObjectId {
        todo!()
    }

    /// Phase 10 — the index beside this pack, if it was opened with one.
    ///
    /// # Caveats
    ///
    /// `None` does not mean "empty pack". It means the index is unknown, and every lookup by id
    /// must treat this pack as unsearchable — see [`PackFile::get`].
    pub fn index(&self) -> Option<&PackIndex<'a>> {
        self.index.as_ref()
    }

    /// Phase 10 — how many objects the pack header claims.
    ///
    /// # Caveats
    ///
    /// The header's count, which is a claim about the pack as it was written. It must equal the
    /// index's [`PackIndex::len`] for the two files to describe each other, and it bounds only a
    /// sequential walk — a lookup by offset never consults it.
    pub fn object_count(&self) -> u32 {
        todo!()
    }

    /// Phase 10 — whether the object count came from a header that agrees with the index.
    ///
    /// # Caveats
    ///
    /// A pack can be internally consistent without this: an index that describes a *different*
    /// pack has the same layout and different contents, and only this comparison and the shared
    /// pack checksum detect it. A pack with no index returns `false` — not because the count is
    /// wrong, but because there is nothing to agree with, and callers that require a trustworthy
    /// pack should check the index is present separately.
    pub fn is_index_consistent(&self) -> bool {
        self.index.as_ref().map(|index| index.len() == self.object_count() as usize).unwrap_or(false)
    }

    /// Phase 10 — read one object by id, resolving its delta chain.
    ///
    /// The normal read path. Everything an object database does on a packed repository funnels
    /// through here.
    ///
    /// # Caveats
    ///
    /// * **The object is hash-verified before it is returned.** A packed object whose content
    ///   hashes to something other than the id it was found under is
    ///   [`crate::error::Error::HashMismatch`], and this is the only place that can catch it —
    ///   after a delta is applied, the entry bytes have no relationship to the id at all. The plan
    ///   requires this check on every read (§5), so it is not conditional on a debug flag.
    /// * **Not found is not an error.** An object absent from this pack is
    ///   [`crate::error::Error::ObjectNotFound`] only if the caller knows no other store has it;
    ///   within [`crate::odb::Odb`] the absence is used to try the next store, so a lookup that
    ///   misses must be cheap and must not be a special error variant.
    /// * **A thin base is not reported here.** If a `REF_DELTA` base is missing, the error is
    ///   [`crate::error::Error::MissingDeltaBase`], and turning that into
    ///   [`crate::error::Error::ThinPack`] requires knowing whether the object exists elsewhere —
    ///   which is the object database's knowledge, not this type's.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if the index does not describe this id or the pack
    /// has no index; [`crate::error::Error::CorruptPack`] for a bad entry header, a bad zlib
    /// stream, or an offset outside the pack; [`crate::error::Error::CorruptDelta`],
    /// [`crate::error::Error::DeltaChainTooDeep`] and
    /// [`crate::error::Error::MissingDeltaBase`] from the resolver; and
    /// [`crate::error::Error::HashMismatch`] when the content does not match the id.
    pub fn get(&self, oid: &ObjectId, bases: Option<&dyn BaseProvider>) -> Result<Object> {
        todo!()
    }

    /// Phase 10 — read the object a specific entry describes, resolving its delta chain.
    ///
    /// # Caveats
    ///
    /// * **The offset is trusted to be an entry boundary.** One that points mid-entry is caught
    ///   when the header decodes — usually as a plausible-looking small size, and eventually as a
    ///   bad zlib stream — rather than immediately, so callers must not treat a `CorruptPack` here
    ///   as proof that the offset was the problem. Offsets come from the `.idx` and from
    ///   `OFS_DELTA` distances, both of which are verified against the pack during `index-pack`.
    /// * **No hash check is possible.** The caller supplied an offset, not an id, so there is
    ///   nothing to compare against. [`PackFile::get`] is the checked path, and a caller that reads
    ///   by offset has taken on the verification itself.
    /// * **`bases` supplies what this pack does not contain.** A `REF_DELTA` may name an object
    ///   that was never sent — that is what a thin pack is — and no amount of reading the pack will
    ///   find it. Passing `None` is correct only for a pack known to be self-contained; it turns a
    ///   thin base into [`crate::error::Error::MissingDeltaBase`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`], [`crate::error::Error::CorruptDelta`],
    /// [`crate::error::Error::DeltaChainTooDeep`] and
    /// [`crate::error::Error::MissingDeltaBase`] as in [`PackFile::get`], but never
    /// [`crate::error::Error::HashMismatch`] or [`crate::error::Error::ObjectNotFound`] — a missing
    /// base is reported as a missing base, not as a missing object, because the caller needs to
    /// tell a thin pack from a corrupt one.
    pub fn get_at_offset(
        &self,
        offset: u64,
        bases: Option<&dyn BaseProvider>,
    ) -> Result<(ObjectKind, Vec<u8>)> {
        todo!()
    }

    /// Phase 10 — decode the entry header at `offset`, without inflating anything.
    ///
    /// This is the cheap question: the type, the declared size, and for a delta the base naming.
    /// `verify-pack` reports these for every entry, and the sequential walk steps from one entry's
    /// `data_offset` plus its inflated length to the next.
    ///
    /// # Caveats
    ///
    /// * **[`PackEntry::size`] is the entry's own payload length**, which for a delta is the
    ///   instruction stream's length and not the object's. This function cannot know better — the
    ///   object's length is inside the delta payload, which it has deliberately not read.
    /// * The returned `data_offset` is where the zlib stream starts, which is what a sequential
    ///   reader inflates from; it is not the entry's offset and the two differ by the header's
    ///   variable length.
    /// * Decoding does **not** validate that the base exists, or that the offset is inside the
    ///   pack — only that a header can be read there. Both are the caller's next step.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`] if `offset` is past the end, the header is truncated,
    /// the size varint overflows, or the type nibble is one of the two reserved values.
    pub fn entry_at(&self, offset: u64) -> Result<PackEntry> {
        todo!()
    }

    /// Phase 10 — walk entries sequentially from the start of the pack.
    ///
    /// The only way to enumerate a pack that has no index, and the shape `index-pack` and
    /// `verify-pack` both want: entries in file order, each one's end discovered by inflating it.
    ///
    /// # Caveats
    ///
    /// * **Inflates every entry**, including the ones it does not return as data, because the
    ///   boundary of the next entry is only knowable by consuming this entry's zlib stream. There
    ///   is no cheap skip.
    /// * **Stops at the trailer**, which it identifies from the header's object count *and* the
    ///   checksum's position — a pack whose count is wrong would otherwise walk into the trailer
    ///   and try to inflate checksum bytes as an entry header.
    /// * The iterator yields the entry **and** its inflated payload, because both callers need the
    ///   payload and inflating twice is the expensive part. A caller that only wants headers should
    ///   call [`PackFile::entry_at`] as it steps, and pay the inflation anyway.
    ///
    /// # Errors
    ///
    /// The first failure ends the iteration: [`crate::error::Error::CorruptPack`] for a bad header
    /// or zlib stream, and [`crate::error::Error::TrailingPackBytes`] if the count and the
    /// trailer disagree about where the entries stop. A `Result` per item, not a single `Result`
    /// around the iterator, because a walk that fails at entry 40 000 must still be able to report
    /// the 39 999 it got through.
    pub fn iter_entries(&self) -> PackReader<'_> {
        todo!()
    }

    /// Phase 10 — the byte range one entry occupies in the pack, compressed.
    ///
    /// The range an `.idx`'s CRC covers, and nothing else — see [`crate::pack::crc32`] for why
    /// that range and not the inflated payload. This needs the entry's inflated length to find the
    /// end, so it is not free.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`] if the entry cannot be read, or if its zlib stream runs
    /// past the trailer.
    pub fn entry_range(&self, offset: u64) -> Result<Range<usize>> {
        todo!()
    }

    /// Phase 10 — hash the pack and compare against its trailer.
    ///
    /// # Caveats
    ///
    /// Linear in the pack's size and reads every byte, so it is a `verify-pack` and `fsck`
    /// operation, never something a normal read pays for. It is also the only check that covers
    /// bytes no entry claims — corruption in the middle of a zlib stream that still happens to
    /// inflate is caught per object, but a whole pack that was truncated and re-hashed is only
    /// caught here against the `.idx`'s recorded checksum.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`] if the recomputed checksum differs from the trailer,
    /// and [`crate::error::Error::Io`] from the hasher.
    pub fn verify_checksum(&self) -> Result<()> {
        todo!()
    }
}

/// Phase 10 — a [`PackFile`] that owns its buffers, so it can be stored.
///
/// # Caveats
///
/// A self-referential pair by construction: the pack borrows the `Vec` that holds it. Access goes
/// through [`OwnedPackFile::pack`], which borrows for one call rather than handing out a
/// `&PackFile<'static>` — the `Deref` shortcut would be unsound, exactly as it would be for
/// [`crate::pack::OwnedPackIndex`].
///
/// There is a real cost to the alternative this type is avoiding: a caller holding a `PackFile`
/// over a `Vec` it also owns has to keep both in a struct, which Rust will not let it do directly.
/// Reading the files into `Vec`s and wrapping the pair in this value is that pattern, made safe.
#[derive(Clone, Debug)]
pub struct OwnedPackFile {
    /// The owned `.pack` bytes.
    data: Vec<u8>,
    /// The owned `.idx` bytes, when there was one.
    index_data: Option<Vec<u8>>,
    /// The repository's id width.
    format: ObjectFormat,
}

impl OwnedPackFile {
    /// Phase 10 — take ownership of a pack and its optional index.
    ///
    /// # Caveats
    ///
    /// Parses nothing up front: the `.idx` checksum is verified by [`OwnedPackFile::pack`] on the
    /// first call, and a corrupt index therefore surfaces at the first read rather than at
    /// construction. A caller that wants to fail early — `Odb::open`, which must not defer an
    /// error past the point it can report a path — calls `pack()` once immediately.
    pub fn new(data: Vec<u8>, index_data: Option<Vec<u8>>, format: ObjectFormat) -> Self {
        Self {
            data,
            index_data,
            format,
        }
    }

    /// Phase 10 — the pack's own bytes, and the index's, as one borrowed [`PackFile`].
    ///
    /// # Errors
    ///
    /// Whatever [`PackFile::open`] and [`PackIndex::parse`] raise, re-checked on each call because
    /// the borrow cannot be cached in this value.
    pub fn pack(&self) -> Result<PackFile<'_>> {
        todo!()
    }
}

/// Phase 10 — a sequential walk over a pack's entries, in file order.
///
/// Yielded by [`PackFile::iter_entries`].
///
/// # Caveats
///
/// * **Items are `Result`s.** The walk cannot know it has hit corruption until it tries to read
///   the next entry, and a single `Result` around the whole iterator would discard the entries that
///   did read cleanly. `index-pack` needs exactly this: the entries before the failure are real
///   and reportable.
/// * **Each item carries the inflated payload**, so a caller iterating a large pack holds one
///   entry's worth of decompressed bytes at a time. A `PackReader` that collected its items would
///   hold the whole pack decompressed, which for a source tarball is several times the `.pack` in
///   memory.
/// * **Exhausted after the first error?** No — the position is left where it stopped, so calling
///   `next` again re-reports the same failure rather than silently moving on. Advancing past a
///   corrupt entry is not possible in this format: there is no way to resynchronise, because entry
///   boundaries are only discoverable by decompressing.
#[derive(Clone, Debug)]
pub struct PackReader<'a> {
    /// The pack being walked.
    pack: PackFile<'a>,
    /// Absolute offset of the next entry to read.
    next_offset: u64,
    /// How many entries have been produced, against the header's count.
    produced: u32,
}

impl<'a> Iterator for PackReader<'a> {
    /// Phase 10 — the next entry and its inflated payload, or the failure that stopped the walk.
    ///
    /// # Caveats
    ///
    /// The payload's meaning depends on [`PackEntry::kind`]: object content for an undeltified
    /// entry, an instruction stream for a delta. A consumer that wants an object must check, and
    /// resolve the delta through [`crate::pack::resolve_delta`] — treating a delta's payload as
    /// content is the mistake this split exists to make visible.
    type Item = Result<(PackEntry, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        todo!()
    }

    /// Phase 10 — how many entries remain, from the pack header's count.
    ///
    /// # Caveats
    ///
    /// The lower bound is exact for a well-formed pack and the count came from the header, so it is
    /// a claim rather than a fact — a pack whose count overstates its contents yields fewer items
    /// than promised, terminating with a [`crate::error::Error::TrailingPackBytes`] item.
    fn size_hint(&self) -> (usize, Option<usize>) {
        todo!()
    }
}
