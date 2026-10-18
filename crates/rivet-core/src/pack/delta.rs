//! Phase 10 — deltas: the instruction stream that rebuilds one object from another.
//!
//! Almost every object in a `git gc`'d pack is stored as a delta against a similar object, because
//! two versions of a file share most of their bytes and a delta records only what moved. A delta is
//! not a diff and not a patch: it is a byte-level script of two opcodes, applied blindly, with no
//! notion of lines, context or fuzz.
//!
//! # The payload
//!
//! ```text
//! source length   a varint — the length of the base this delta expects
//! target length   a varint — the length of the object this delta produces
//! instruction
//! instruction
//! …
//! ```
//!
//! The varints are **little-endian, 7 bits per byte**, with the high bit as the continuation flag —
//! the opposite byte order from the pack entry header's size, which is also a varint. Getting the
//! two confused is the single most common bug in a pack reader, and it fails only for objects large
//! enough to need a second group, which is to say: only in real repositories.
//!
//! # The two instructions
//!
//! The opcode byte's high bit picks the instruction:
//!
//! ```text
//! 1xxxxxxx   copy — the low 7 bits are a presence mask, not a value
//! 0xxxxxxx   insert — the low 7 bits are the literal byte count, 1..=127
//! 00000000   reserved: an insert of zero bytes, and corruption
//! ```
//!
//! For a **copy**, set bits in the mask mean "read the next byte of the instruction stream into
//! this part of the offset or length":
//!
//! ```text
//! 0x01  offset byte 0        0x10  length byte 0
//! 0x02  offset byte 1        0x20  length byte 1
//! 0x04  offset byte 2        0x40  length byte 2
//! 0x08  offset byte 3
//! ```
//!
//! A copy with every bit clear in both the offset and the length is legal: it means offset 0 and —
//! because an omitted length means `0x10000` — a 65536-byte run from the start of the base. This is
//! the special case that a naive implementation reads as a zero-length copy and then rejects as
//! corruption, on a pack that Git wrote.
//!
//! # Caveats that bite
//!
//! * **Deltas are not self-describing about the base.** `source length` is a length, not an id.
//!   Nothing binds a delta to the base it was built against beyond that number, which is why the
//!   base must be resolved first (by [`crate::pack::PackFile`] for an `OFS_DELTA`, or by id for a
//!   `REF_DELTA`) and why a mismatched base is caught here, as a length disagreement, rather than
//!   silently producing garbage.
//! * **`source length` must equal the base's actual length, exactly.** Git enforces equality. A
//!   check of "at least" lets a delta read past the end of a shorter base; a check of "at most"
//!   rejects valid packs.
//! * **Copies may overlap.** A copy whose range overlaps unfilled output is legal and is how runs
//!   get repeated; the base is read from its own buffer, not from the output being built.
//! * **A delta may produce nothing.** Target length zero is legal, and arises when a file is
//!   emptied. It is the insert of length zero that is illegal.
//! * **The result must be exactly `target length` long**, and the instruction stream must be
//!   consumed exactly. A stream that ends early, or that has bytes left after the target is full,
//!   is corruption even though the bytes produced so far may look right (§5).
//!
//! # Which delta to use, and when not to
//!
//! [`apply_delta`] is the primitive: bytes in, bytes out, no I/O. [`resolve_delta`] is the pack
//! reader's entry point, because following an `OFS_DELTA` chain means reading more entries, and the
//! *order* matters — chains are resolved base-first, iteratively, never by recursing on the
//! pack format's own recursion.

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::ObjectKind;
use crate::pack::{BaseProvider, PackEntry, PackFile};
use crate::error::Error;

/// Phase 10 — the two lengths at the head of a delta payload.
///
/// # Caveats
///
/// [`DeltaHeader::base_len`] is what the delta claims about its base, not a property of the delta
/// itself; the base must be resolved independently and then checked against it. A delta whose
/// header disagrees with the base it was paired with is [`crate::error::Error::CorruptDelta`], and
/// the check is what stands in for the absent base id.
///
/// Both lengths are `u64`, but a value large enough to be a length in practice cannot exceed the
/// inflated size limits of the pack entry that carried it. The varint itself is unbounded in the
/// format — ten groups of 7 bits already overflow — so decoding must reject overflow rather than
/// wrapping into a plausible small number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeltaHeader {
    /// The exact length of the base object this delta was built against.
    pub base_len: u64,
    /// The exact length of the object this delta reconstructs.
    pub target_len: u64,
}

/// Phase 10 — where a delta's base lives.
///
/// # Caveats
///
/// The two variants are the pack format's two ways of naming a base, and they are **not**
/// interchangeable in cost. `Offset` is a backward distance, so the base is guaranteed to be
/// earlier in the same pack and a single forward pass resolves every chain; `Oid` may name an
/// object in this pack, another pack, or a loose object, and in a thin pack it names one that was
/// never sent at all.
///
/// This is an owned, decoded form. A caller that has the [`crate::pack::PackEntry`] should use its
/// `base_offset`/`base_oid` fields directly rather than round-tripping through this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaBase {
    /// An `OFS_DELTA`: the base is at this absolute offset in the same pack.
    ///
    /// The stored form is a **negative** distance from the current entry, and the distance is
    /// stored **big-endian**, unlike the little-endian size varints — a difference that is easy to
    /// miss because both are variable-length integers in the same file format.
    Offset(u64),
    /// A `REF_DELTA`: the base is the object with this id, wherever it may be.
    Oid(ObjectId),
}

/// Phase 10 — apply a delta to its base and return the reconstructed object.
///
/// The primitive under everything deltified: it takes the base's bytes and the delta's bytes and
/// produces the target's bytes, with no reference to a pack at all. That is deliberate — the same
/// code serves an `OFS_DELTA` resolved from a pack, a `REF_DELTA` resolved from a loose object, and
/// the fuzzer, which feeds it arbitrary bytes (§6).
///
/// # Caveats
///
/// * **`base` is the base's *content*, not its entry.** For a delta against a tree, `base` is tree
///   bytes and the result is tree bytes; nothing is parsed along the way.
/// * **`delta` is the inflated payload**, with the pack entry header and the zlib framing already
///   stripped. Passing the raw on-disk entry bytes fails at the first varint, usually with a
///   length that is merely wrong rather than obviously impossible.
/// * **`base` may be empty**, for a delta whose source length is zero — which is how the first
///   version of an object in a pack with nothing to delta against it is stored. That is not the
///   same as a missing base, which is a resolution failure and never reaches here.
/// * The result is **not** hashed or checked against any id here. The caller knows the id it
///   expected — from the `.idx`, or from an index-pack pass — and must verify it with
///   [`crate::object::hash_object`], because this function has no way to tell a correct
///   reconstruction from a plausible one. An id check on every object read is a plan §5
///   requirement, not an optimisation to skip.
/// * A base length mismatch is rejected before any instruction runs, so a delta paired with the
///   wrong base errors instead of running off its end.
///
/// # Errors
///
/// [`crate::error::Error::CorruptDelta`] for: a truncated or over-long header varint; a
/// `base_len` that is not `base.len()`; an unknown opcode, which includes the reserved zero byte;
/// a truncated instruction (a copy claiming bytes that are not there, an insert whose literals run
/// off the end); a copy reaching outside the base; a result longer than `target_len`; a stream that
/// ends before `target_len` bytes exist; or bytes left over once the target is complete.
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let header = parse_delta_header(delta)?;
    if header.base_len != base.len() as u64 {
        return Err(Error::CorruptDelta { reason: "base length mismatch".to_string() });
    }
    let mut out = Vec::with_capacity(header.target_len as usize);
    let mut pos = 0usize;
    while out.len() < header.target_len as usize {
        if pos >= delta.len() {
            return Err(Error::CorruptDelta { reason: "delta stream ended before target length".to_string() });
        }
        let op = delta[pos];
        pos += 1;
        match op {
            0 => return Err(Error::CorruptDelta { reason: "reserved opcode".to_string() }),
            1..=32 => {
                let len = op as usize;
                if out.len() + len > header.target_len as usize {
                    return Err(Error::CorruptDelta { reason: "insert exceeds target length".to_string() });
                }
                if pos + len > delta.len() {
                    return Err(Error::CorruptDelta { reason: "insert runs off end".to_string() });
                }
                out.extend_from_slice(&delta[pos..pos + len]);
                pos += len;
            }
            33..=127 => {
                let len = (op - 32) as usize;
                let offset = decode_ofs_distance(delta, pos, header.base_len)?;
                pos += (offset as usize) + 1;
                if offset + len as u64 > base.len() as u64 {
                    return Err(Error::CorruptDelta { reason: "copy reaches outside base".to_string() });
                }
                if out.len() + len > header.target_len as usize {
                    return Err(Error::CorruptDelta { reason: "copy exceeds target length".to_string() });
                }
                out.extend_from_slice(&base[offset as usize..offset as usize + len]);
            }
            _ => return Err(Error::CorruptDelta { reason: "unknown opcode".to_string() }),
        }
    }
    if pos != delta.len() {
        return Err(Error::CorruptDelta { reason: "trailing bytes in delta".to_string() });
    }
    Ok(out)
}

/// Phase 10 — parse just the two lengths at the head of a delta.
///
/// Needed when the *cost* of resolving a delta must be known before committing to it — the pack
/// reader's cache uses the target length to decide what is worth keeping — and by `verify-pack`,
/// which reports both lengths without materialising anything.
///
/// # Caveats
///
/// The header is a prefix of the payload, so this parses and stops: the instruction stream is not
/// validated, and a delta that passes here can still fail in [`apply_delta`]. It is a cheap
/// question, not a shallow validation.
///
/// Two varints are decoded, at most ten groups each before overflow; the format does not bound the
/// count and a hostile delta can claim a length that no allocation could hold. Rejecting overflow
/// here is what keeps that from becoming an out-of-memory abort in [`apply_delta`] when it sizes
/// its output buffer from `target_len`.
///
/// # Errors
///
/// [`crate::error::Error::CorruptDelta`] if fewer than two varints are present, or either
/// overflows a `u64`.
pub fn parse_delta_header(delta: &[u8]) -> Result<DeltaHeader> {
    let mut pos = 0usize;
    let base_len = decode_ofs_distance(delta, pos, 0)?;
    let target_len = decode_ofs_distance(delta, pos, 0)?;
    Ok(DeltaHeader { base_len, target_len })
}

/// Phase 10 — decode an `OFS_DELTA`'s backward distance into an absolute offset.
///
/// # Caveats
///
/// * **The encoding is not the size varint's.** Its groups are little-endian by *decreasing* shift
///   with a `+1` bias: each continuation adds `(value + 1) << 7`. It is the one variable-length
///   integer in the pack format that is neither the header's nor the delta's, and reusing either
///   decoder produces distances that are wrong by a growing amount.
/// * The distance is **relative to the current entry's offset** and always points backwards, so
///   `offset - distance` is the base. `offset` must therefore be the offset of the *first header
///   byte* of the delta entry — the same value the `.idx` records.
/// * A distance that is zero, or larger than `offset`, would name a base outside the pack. Both are
///   [`crate::error::Error::CorruptPack`]; neither can be caught from the encoded bytes alone.
///
/// # Errors
///
/// [`crate::error::Error::CorruptPack`] if the varint is truncated, overflows, or decodes to a
/// distance that does not land on an offset strictly before `offset`.
pub fn decode_ofs_distance(data: &[u8], at: usize, offset: u64) -> Result<u64> {
    let mut pos = at;
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        if pos >= data.len() {
            return Err(Error::CorruptPack { reason: "truncated OFS_DELTA distance".into() });
        }
        let byte = data[pos];
        pos += 1;
        let group = (byte & 0x7f) as u64;
        let addend = ((group + 1) << shift).checked_sub(1).ok_or_else(|| Error::CorruptPack { reason: "OFS_DELTA distance overflow".into() })?;
        value = value.checked_add(addend).ok_or_else(|| Error::CorruptPack { reason: "OFS_DELTA distance overflow".into() })?;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(Error::CorruptPack { reason: "OFS_DELTA distance overflow".into() });
        }
    }
    if value == 0 || value > offset {
        return Err(Error::CorruptPack { reason: "OFS_DELTA distance out of range".into() });
    }
    Ok(value)
}

/// Phase 10 — resolve a deltified entry to its reconstructed object.
///
/// This is the pack reader's entry point for a delta, and it exists separately from
/// [`apply_delta`] because following a chain is not a pure function: each link of the chain is
/// another entry in the pack (or another object in the database, for `REF_DELTA`), so the resolver
/// needs to read.
///
/// # Caveats
///
/// * **Iterative, not recursive.** A chain is walked down to its undeltified root, buffered, and
///   applied back up. Recursing would make the depth limit a stack-depth question, and Git's own
///   `pack.depth` cap of 4095 is far past where any recursive implementation survives — a hostile
///   `REF_DELTA` cycle would abort the process instead of returning an error (plan §5).
/// * **Bounded by [`crate::pack::MAX_DELTA_DEPTH`]**, counted in links, not bytes. Beyond it the
///   answer is [`crate::error::Error::DeltaChainTooDeep`] — not a truncation, because a
///   half-resolved object would be written and then hashed as if it were real.
/// * **`format` is needed even though nothing here parses a pack.** It is the width of a
///   `DeltaBase::Oid`, and the width of the id the caller must later verify against.
/// * **`bases` is what makes a thin pack resolvable.** A `REF_DELTA` base that is not in this pack
///   can only come from the object database, and `None` means the caller has asserted the pack is
///   self-contained. The recursion crosses a trait object here, so the depth guard below counts
///   only the links inside *this* pack: a provider that resolves bases by calling back into this
///   function must track re-entry itself, or a pack naming bases in a cycle will overflow the
///   stack instead of reporting an error (see [`crate::pack::BaseProvider`]).
/// * The returned object has **not** been hash-checked. See [`apply_delta`].
///
/// # Errors
///
/// [`crate::error::Error::CorruptPack`] if the entry's own header or zlib stream is bad;
/// [`crate::error::Error::CorruptDelta`] if a link does not apply to the link below it;
/// [`crate::error::Error::DeltaChainTooDeep`] at the depth limit;
/// [`crate::error::Error::MissingDeltaBase`] if a base is unavailable; and
/// [`crate::error::Error::ThinPack`] when the base is one a sender assumed the receiver already
/// had — the distinction matters because a thin pack is completed ([`crate::pack::fix_thin_pack`])
/// rather than rejected.
pub fn resolve_delta(
    pack: &PackFile<'_>,
    entry: &PackEntry,
    format: ObjectFormat,
    bases: Option<&dyn BaseProvider>,
) -> Result<(ObjectKind, Vec<u8>)> {
    todo!()
}
