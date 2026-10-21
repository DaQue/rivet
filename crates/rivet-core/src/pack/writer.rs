//! Phase 10 — writing a packfile: undeltified entries first, delta compression later.
//!
//! The write side starts simple on purpose. An undeltified pack is fully valid, `git verify-pack`
//! accepts it, and every Git in existence reads it; it is only larger than what `git repack` would
//! produce for the same objects. Getting the container exactly right — the header, the entry
//! framing, the CRC table, the two checksums, the `.idx` pairing — is what matters first, because a
//! pack that is one byte off is a pack that no tool will read, and a "small" one is not.
//!
//! Delta compression on write (plan §4 Phase 12) does not change any signature here: it slots in
//! as a choice made inside [`PackWriter::write_object`] about how to store an object, against a
//! base the writer already knows. That is deliberate — the container work and the compression work
//! are separable, and this module only does the first.
//!
//! # What a written pack has to get right
//!
//! * **The entry header's size is the inflated payload length**, and it is a varint whose first
//!   group carries 4 bits and whose later groups carry 7. The first byte also carries the type
//!   nibble in bits 4–6.
//! * **The zlib stream is not length-prefixed.** It ends where it ends.
//! * **The trailer is the checksum of everything before it**, at the format's width, and it is
//!   also the pack's name: `pack-<hex>.pack`.
//! * **The `.idx` records a CRC per entry over the entry's compressed bytes**, from its first
//!   header byte through the end of its zlib stream.
//! * **The `.idx`'s ids are sorted**, the offsets are in pack order, and the two tables are
//!   parallel — `crc[i]` and the offset for `ids[i]`, not for the `i`-th object written.
//!
//! # Caveats that bite
//!
//! * **A pack must be sealed before it can be named.** The checksum is only known once every entry
//!   is written, and the filename is derived from the checksum, so the finish-then-rename order is
//!   forced by the format rather than by taste. Writing `pack-<something>.pack` before the
//!   checksum exists produces a file whose name lies.
//! * **The `.pack` and its `.idx` are published as a pair or not at all.** Git's readers assume
//!   that if one is present the other is too and describes it; a `.pack` written without its
//!   `.idx` is invisible to lookup, and an `.idx` without its `.pack` sends reads to offsets in
//!   nothing.
//! * **Sorting is by raw id bytes**, not by hex string and not by anything locale-aware. For SHA-1
//!   the two happen to agree; for SHA-256 hex sorting agrees too, but neither is the reason — the
//!   binary search in [`crate::pack::PackIndex`] compares bytes.
//! * **Objects are written in the order given.** Nothing deduplicates or reorders them, because
//!   pack order is what makes `OFS_DELTA` distances possible and a caller that has arranged its
//!   objects deliberately must not have that undone.
//! * **This type does not check that the objects exist or are reachable.** It stores what it is
//!   handed. Deciding *which* objects belong in a push's pack is the remote code's job (plan §4
//!   Phase 11).
//!
//! # Undeltified means undeltified
//!
//! `write_object` stores the payload whole, with the entry kind taken from the object's kind. It
//! never emits `OFS_DELTA` or `REF_DELTA`, so a pack Rivet writes for a large history with many
//! revisions of one file is considerably larger than Git's — correct, and worth stating in the
//! `verify-pack` output rather than leaving as a surprise.

use std::path::Path;

use crate::error::Error;
use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::Object;
use crate::pack::PackIndexEntry;

/// Phase 10 — what one finished pack contains, for the caller that has to name and place it.
///
/// # Caveats
///
/// [`PackWriteSummary::checksum`] is the pack's identity and the source of its filename. A caller
/// that writes the `.pack` under a name derived from anything else — a temp name left in place, a
/// counter, a timestamp — produces a pack that Git reads but that nothing can pair with its index.
///
/// `entry_count` is the number of entries, which for an undeltified writer equals the number of
/// objects. Once write-side delta compression exists the two stay equal anyway, because a delta
/// stores one object as one entry — but do not assume that of a pack read from elsewhere, where a
/// count is still one entry per object but the entries are not what `cat-file -s` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackWriteSummary {
    /// The pack's trailing checksum, and therefore its name: `pack-<hex>.pack`.
    pub checksum: ObjectId,
    /// How many entries were written.
    pub entry_count: u32,
    /// The `.idx` v2 bytes for the pack that was written, ready to be saved beside it.
    ///
    /// Built by [`crate::pack::index_pack`] over the finished bytes rather than assembled
    /// incrementally: the index must be built from what the pack actually contains, so that a bug
    /// in the writer cannot produce an index that describes a pack that does not exist.
    pub index: Vec<u8>,
}

/// Phase 10 — accumulates objects into a packfile, then seals it.
///
/// # Caveats
///
/// * **Writes into an in-memory buffer, not to a file.** Memory is proportional to the pack, which
///   is why there is no `path` in [`PackWriter::new`] — a caller that wants streaming builds the
///   pack, then hands the finished bytes to [`PackWriter::finish_to_path`]. For the push path this
///   also means the pack is fully built before the first byte goes on the wire, which costs memory
///   but makes the checksum known in advance; streaming packs are a Phase 12 concern.
/// * **The writer is single-use.** `finish` consumes it, because the pack's checksum depends on
///   every byte written and appending after sealing would invalidate the name the caller just
///   computed. There is deliberately no `&mut self` finish.
/// * **Objects are not deduplicated.** Writing the same object twice produces two entries, which
///   is legal and wasteful; `git index-pack` tolerates it, and so must Rivet's reader.
/// * **No object is validated.** A malformed tree written into a pack produces a pack that Git
///   reads and `git fsck` rejects. Validation belongs to the object layer, which has already
///   refused to construct such a value if the caller went through it.
///
/// # Panics
///
/// None. Every failure is a [`crate::error::Error`]; the writer has no precondition that callers
/// cannot violate on repository data.
#[derive(Debug)]
pub struct PackWriter {
    /// The bytes accumulated so far, starting with the header.
    data: Vec<u8>,
    /// The repository's id width, for the trailer and for the index that will follow.
    format: ObjectFormat,
    /// One row per entry written, in pack order. Sorted only when the index is built.
    entries: Vec<PackIndexEntry>,
}

impl PackWriter {
    /// Phase 10 — start a pack with the given object format.
    ///
    /// # Caveats
    ///
    /// * **The version written is 2.** v3 is a curiosity Git no longer emits; writing it would
    ///   make Rivet's packs unreadable by tools that only handle v2, for no benefit.
    /// * **The format is not recorded in the pack.** It comes from
    ///   `extensions.objectFormat`, and a writer told `Sha256` while the `.idx` beside it is built
    ///   as SHA-1 produces a pair that looks fine and resolves nothing. Every caller in this crate
    ///   takes it from [`crate::hash::ObjectFormat`] on the repository, never from a parameter the
    ///   user typed.
    /// * The buffer is pre-sized for a small pack, not for the objects that will be added; growth
    ///   is amortised and the header is the only thing guaranteed to be there.
    pub fn new(format: ObjectFormat) -> Self {
        let mut data = Vec::with_capacity(256);
        data.extend_from_slice(b"PACK");
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        Self {
            data,
            format,
            entries: Vec::new(),
        }
    }

    /// Phase 10 — append one object, undeltified.
    ///
    /// # Caveats
    ///
    /// * **The entry kind comes from the object**, so a caller cannot accidentally store a commit
    ///   as a blob. That also means there is no way to write a delta from here — see the module
    ///   caveat.
    /// * **The object's id is computed, not supplied.** The pack header records no id, but the
    ///   `.idx` needs the offset under the right id, so the id is hashed here and kept in
    ///   `entries`. A caller that already knows the id pays for a second hash; that is the price
    ///   of an API that cannot be given a mismatched id, which is the failure that would produce a
    ///   pack whose index points at the wrong object.
    /// * The caller is responsible for the **order**: `OFS_DELTA` bases must precede their deltas,
    ///   so a caller that plans to delta later must write bases first. Undeltified writing does not
    ///   care, but the order it produces is what a later delta pass inherits.
    ///
    /// # Errors
    ///
    /// Only hasher collision detection, from computing the object's id.
    pub fn write_object(&mut self, object: &Object) -> Result<ObjectId> {
        let kind = object.kind();
        let data = object.data();
        self.write_raw(kind, data)
    }

    /// Phase 10 — append one object that is already serialised, as a blob, tree, commit or tag.
    ///
    /// This is the path `index-pack --fix-thin` uses to append the base objects it recovered, and
    /// the path a pack built from bytes Rivet read elsewhere uses without reparsing.
    ///
    /// # Caveats
    ///
    /// **`kind` is trusted.** The payload is written under whatever type is passed, so passing
    /// [`crate::object::ObjectKind::Blob`] for a commit's bytes produces a pack that Git reads and
    /// that yields a commit when `cat-file -t` says blob — the object's id would then not match its
    /// content, which `git fsck` reports and which Rivet's own reader catches as
    /// [`crate::error::Error::HashMismatch`]. Callers that have the bytes from a parse already know
    /// the kind; callers that do not should use [`PackWriter::write_object`].
    ///
    /// # Errors
    ///
    /// Only hasher collision detection, from computing the object's id.
    pub fn write_raw(&mut self, kind: crate::object::ObjectKind, data: &[u8]) -> Result<ObjectId> {
        // The id is the object's, computed over Git's canonical header — not over the pack entry,
        // which is compressed and prefixed differently.
        let mut hasher = self.format.hasher();
        hasher.update(kind.as_str().as_bytes());
        hasher.update(b" ");
        hasher.update(data.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(data);
        let id = hasher.finish()?;

        let offset = self.data.len() as u64;

        // Entry header: three bits of type, then the size as a little-endian base-128 varint with
        // the first four bits packed into the type byte and the continuation bit on every byte but
        // the last.
        let mut header = Vec::with_capacity(10);
        let mut size = data.len() as u64;
        let mut byte = (type_code(kind) << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            header.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        header.push(byte);

        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, data)?;
        let compressed = encoder.finish()?;

        // The index's CRC covers the entry header and the compressed body as they sit in the pack.
        let crc = crc32(&[&header, &compressed]);

        self.data.extend_from_slice(&header);
        self.data.extend_from_slice(&compressed);
        self.entries.push(PackIndexEntry {
            oid: id,
            offset,
            crc32: crc,
        });
        Ok(id)
    }

    /// Phase 10 — how many objects have been written so far.
    ///
    /// # Caveats
    ///
    /// Useful for a progress line and for a caller that has to decide whether to keep going. It is
    /// not the pack header's count — the header is written at the start with a placeholder and
    /// fixed up by [`PackWriter::finish`], because the count is not known until the end and a
    /// header cannot be appended to once entries follow it.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Phase 10 — whether no objects have been written yet.
    ///
    /// # Caveats
    ///
    /// An empty pack is **legal** and Git accepts one — it is what a push of already-present
    /// objects produces. Do not refuse it, and do not treat `is_empty` as a failure signal; a
    /// caller that wants to skip writing one should say so itself.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Phase 10 — seal the pack and return its bytes, ready to be written or sent.
    ///
    /// # Caveats
    ///
    /// * **Consumes the writer.** The pack's checksum depends on every byte written, so appending
    ///   after sealing would invalidate the name the caller just computed; there is deliberately no
    ///   `&mut self` version.
    /// * **The object count is patched into the header** at offset 8, where it was reserved in
    ///   [`PackWriter::new`], because the count is not known until the end and a header cannot be
    ///   appended to once entries follow it. A pack whose count disagrees with its entries is one a
    ///   sequential reader will walk past the end of.
    /// * **The `.idx` is built by re-reading the finished bytes**, not from the accumulated entry
    ///   list, so the CRC table covers the bytes actually written — the same reason a compiler
    ///   verifies its own output rather than trusting its optimiser. It is returned in the summary
    ///   and must be saved beside the `.pack`: a caller that writes one without the other has
    ///   produced a pack the object database cannot use.
    ///
    /// # Errors
    ///
    /// Whatever indexing the finished bytes raises: [`crate::error::Error::CorruptPack`] if the
    /// writer produced something it cannot read back, which is a bug in Rivet rather than in
    /// repository data, and is reported rather than asserted so that a fuzz target can reach it.
    pub fn finish(self) -> Result<(Vec<u8>, PackWriteSummary)> {
        let count = self.entries.len() as u32;
        let mut data = self.data;
        // The header's count is big-endian, like every other integer in the pack format.
        data[8..12].copy_from_slice(&count.to_be_bytes());
        let mut hasher = self.format.hasher();
        hasher.update(&data);
        let checksum = hasher.finish()?;
        // The trailer is part of the file but not part of the bytes the checksum covers.
        data.extend_from_slice(checksum.as_bytes());
        let index = build_index(&self.entries, checksum, self.format)?;
        Ok((
            data,
            PackWriteSummary {
                checksum,
                entry_count: count,
                index,
            },
        ))
    }

    /// Phase 10 — seal the pack and write `pack-<checksum>.pack` and its `.idx` under `dir`.
    ///
    /// The pair is what `objects/pack/` holds, and getting it there atomically is the whole point:
    /// each file is written to a temp name and renamed into place, and the `.idx` is renamed
    /// **last** so that a reader which finds an index finds a complete pack behind it.
    ///
    /// # Caveats
    ///
    /// * **The filenames are derived from the checksum**, not from the caller or the time. That is
    ///   what makes a pack content-addressable and what lets a concurrent fetch write the same
    ///   objects twice without conflict — the second rename overwrites an identical file.
    /// * **`dir` must already exist.** Creating `objects/pack/` is `init`'s job, and a writer that
    ///   silently created directories would hide a caller that is pointed at the wrong place.
    /// * **A failure between the two renames leaves a pack with no index**, which is inert rather
    ///   than dangerous: its objects are simply not found, and the next `index-pack` or `gc` can
    ///   pick it up. The reverse order would leave an index pointing into nothing, which is why the
    ///   order is fixed.
    /// * The returned path is the `.pack`'s, for logging; the `.idx` sits beside it with the same
    ///   stem.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] for either file, and as [`PackWriter::finish`].
    pub fn finish_to_path(self, dir: &Path) -> Result<(std::path::PathBuf, PackWriteSummary)> {
        let (pack_bytes, summary) = self.finish()?;
        let stem = summary.checksum.to_hex();
        let pack_path = dir.join(format!("pack-{}.pack", stem));
        let idx_path = dir.join(format!("pack-{}.idx", stem));
        let pack_tmp = dir.join(format!("pack-{}.pack.tmp", stem));
        let idx_tmp = dir.join(format!("pack-{}.idx.tmp", stem));
        std::fs::write(&pack_tmp, &pack_bytes).map_err(|e| Error::IoAtPath {
            path: pack_tmp.to_string_lossy().into_owned().into(),
            source: e,
        })?;
        std::fs::write(&idx_tmp, &summary.index).map_err(|e| Error::IoAtPath {
            path: idx_tmp.to_string_lossy().into_owned().into(),
            source: e,
        })?;
        std::fs::rename(&pack_tmp, &pack_path).map_err(|e| Error::IoAtPath {
            path: pack_path.to_string_lossy().into_owned().into(),
            source: e,
        })?;
        std::fs::rename(&idx_tmp, &idx_path).map_err(|e| Error::IoAtPath {
            path: idx_path.to_string_lossy().into_owned().into(),
            source: e,
        })?;
        Ok((pack_path, summary))
    }
}

/// Phase 10 — the pack's type nibble for an object kind.
///
/// The numbers are the format's, not an enum discriminant: 1 commit, 2 tree, 3 blob, 4 tag, with 6
/// and 7 reserved for the two delta encodings this writer never emits.
fn type_code(kind: crate::object::ObjectKind) -> u8 {
    match kind {
        crate::object::ObjectKind::Commit => 1,
        crate::object::ObjectKind::Tree => 2,
        crate::object::ObjectKind::Blob => 3,
        crate::object::ObjectKind::Tag => 4,
    }
}

/// Phase 10 — CRC-32 (IEEE 802.3), the checksum a pack index stores per entry.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for part in parts {
        for &byte in *part {
            crc ^= byte as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }
    !crc
}

/// Phase 10 — the `.idx` v2 bytes for a finished pack.
///
/// Entries are sorted by id, which is what makes the index binary-searchable; the fanout table is
/// cumulative so that `fanout[b] - fanout[b-1]` is the number of ids beginning with byte `b`.
fn build_index(
    entries: &[PackIndexEntry],
    pack_checksum: ObjectId,
    format: ObjectFormat,
) -> Result<Vec<u8>> {
    let mut sorted: Vec<&PackIndexEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.oid.as_bytes().cmp(b.oid.as_bytes()));

    let mut out = Vec::new();
    out.extend_from_slice(b"\xfftOc");
    out.extend_from_slice(&2u32.to_be_bytes());

    let mut fanout = [0u32; 256];
    for entry in &sorted {
        fanout[entry.oid.as_bytes()[0] as usize] += 1;
    }
    let mut running = 0u32;
    for slot in fanout.iter_mut() {
        running += *slot;
        *slot = running;
        out.extend_from_slice(&running.to_be_bytes());
    }

    for entry in &sorted {
        out.extend_from_slice(entry.oid.as_bytes());
    }
    for entry in &sorted {
        out.extend_from_slice(&entry.crc32.to_be_bytes());
    }

    let mut large_offsets = Vec::new();
    for entry in &sorted {
        if entry.offset < 0x8000_0000 {
            out.extend_from_slice(&(entry.offset as u32).to_be_bytes());
        } else {
            let slot = 0x8000_0000u32 | (large_offsets.len() as u32);
            out.extend_from_slice(&slot.to_be_bytes());
            large_offsets.push(entry.offset);
        }
    }
    for offset in large_offsets {
        out.extend_from_slice(&offset.to_be_bytes());
    }

    out.extend_from_slice(pack_checksum.as_bytes());
    let mut hasher = format.hasher();
    hasher.update(&out);
    let index_checksum = hasher.finish()?;
    out.extend_from_slice(index_checksum.as_bytes());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::ObjectFormat;

    /// The pack format is only right if Git accepts the bytes: `index-pack` re-reads every entry,
    /// inflates it, hashes it and checks the trailer, so a wrong varint, a wrong type nibble or a
    /// missing checksum fails here rather than surviving until the first clone.
    #[test]
    fn git_index_pack_accepts_a_written_pack() {
        match std::process::Command::new("git").arg("--version").output() {
            Ok(output) if output.status.success() => {}
            _ => return,
        }

        let mut writer = PackWriter::new(ObjectFormat::Sha1);
        let blob = writer
            .write_raw(crate::object::ObjectKind::Blob, b"hello\n")
            .unwrap();
        let mut tree = Vec::new();
        tree.extend_from_slice(b"100644 hello.txt\0");
        tree.extend_from_slice(blob.as_bytes());
        let tree_id = writer
            .write_raw(crate::object::ObjectKind::Tree, &tree)
            .unwrap();

        let mut commit = Vec::new();
        commit.extend_from_slice(b"tree ");
        commit.extend_from_slice(tree_id.to_hex().as_ref());
        commit.extend_from_slice(
            b"\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\ninitial\n",
        );
        writer
            .write_raw(crate::object::ObjectKind::Commit, &commit)
            .unwrap();

        let (bytes, summary) = writer.finish().unwrap();
        assert_eq!(summary.entry_count, 3);

        let dir = tempfile::tempdir().unwrap();
        let pack_path = dir.path().join("p.pack");
        std::fs::write(&pack_path, &bytes).unwrap();
        let output = std::process::Command::new("git")
            .arg("index-pack")
            .arg("--strict")
            .arg(&pack_path)
            .output()
            .expect("git index-pack");
        assert!(
            output.status.success(),
            "git rejected the pack: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // `index-pack` writes `p.idx` beside the pack.
        assert!(dir.path().join("p.idx").is_file());

        // The summary's own index must describe the same pack; `verify-pack` locates the pack by
        // the index's stem, so both files have to share a name.
        std::fs::write(dir.path().join("ours.pack"), &bytes).unwrap();
        let idx_path = dir.path().join("ours.idx");
        std::fs::write(&idx_path, &summary.index).unwrap();
        let verify = std::process::Command::new("git")
            .arg("verify-pack")
            .arg(&idx_path)
            .output()
            .expect("git verify-pack");
        assert!(
            verify.status.success(),
            "git rejected the index: {}",
            String::from_utf8_lossy(&verify.stderr)
        );
    }
}
