//! Phase 1 — the blob: a file's contents, and the simplest of the four object kinds.
//!
//! A blob is **uninterpreted bytes**. Git does not record a file name, a mode, a newline
//! convention or an encoding in a blob, and neither may Rivet: those live in the tree entry
//! that points at the blob. Two files with identical contents and different names share one
//! blob, and that deduplication is the whole point.
//!
//! # Caveats that bite
//!
//! * **Symlinks are blobs.** A symlink's blob body is the link target as raw bytes, and its
//!   mode (`120000`) is what distinguishes it. Nothing in the blob itself says "this is a
//!   link", so a parser that classifies by content alone will get symlinks wrong.
//! * **No trailing-newline normalisation, ever.** `core.autocrlf` and line-ending conversion
//!   are explicitly out of scope for v1 (plan §1). A blob's bytes are the bytes.
//! * **Empty blobs are legal and common.** The empty blob is
//!   `e69de29bb2d1d6434b8b29ae775ad8c2e48c5391` in every SHA-1 repository in existence, which
//!   makes it a convenient check for a hashing implementation.
//! * **Binary content is legal.** A NUL byte in a blob is not an error — it only matters to
//!   `diff`, which uses it to decide "binary files differ".

use crate::error::Result;
use crate::hash::ObjectFormat;
use crate::hash::ObjectId;

/// Phase 1 — a file's contents, borrowed from the buffer it was read out of.
///
/// The lifetime keeps the payload borrowed rather than copied, so hashing and writing a large
/// file costs no extra allocation — the bytes go straight from the decompressed loose-object
/// buffer or the mmap'd pack entry into the hasher.
///
/// # Caveats
///
/// This type is for **reading**. To build a blob from a work tree file and hand it to the
/// object database, the data is not in memory yet — use [`Blob::from_reader`], which streams,
/// rather than reading the file into a `Vec` to satisfy this struct's invariant.
///
/// The `data` field is `pub`, so a caller can construct a `Blob` around any byte slice. That is
/// deliberate — it lets tests and `hash-object` avoid a parser round trip — but it means a
/// `Blob` makes **no guarantee** that its contents are well-formed anything. A blob cannot be
/// malformed; the fact that this is the only object kind of which that is true is not a licence
/// to skip validation elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Blob<'a> {
    /// The file's raw contents. May be empty, may contain NUL, is never newline-normalised.
    pub data: &'a [u8],
}

impl<'a> Blob<'a> {
    /// Phase 1 — the object kind of everything this type represents.
    ///
    /// # Caveats
    ///
    /// Always [`crate::object::ObjectKind::Blob`]. The method exists so that the four object
    /// types can be handled generically without the caller matching on which one it has.
    pub const fn kind() -> crate::object::ObjectKind {
        todo!()
    }

    /// Phase 1 — wrap a byte slice, borrowing it.
    ///
    /// # Caveats
    ///
    /// No validation and no copy. The returned blob's id is whatever the bytes hash to, so this
    /// is safe to call on arbitrary input — including input that is not a file's contents at
    /// all.
    pub const fn new(data: &'a [u8]) -> Self {
        todo!()
    }

    /// Phase 1 — the number of bytes in the blob.
    ///
    /// # Caveats
    ///
    /// This is the length that goes in the object header — `"blob <len>\0"`. The header is
    /// hashed, so an off-by-one here does not corrupt the file, it changes the object's
    /// identity and makes it unfindable.
    pub const fn len(&self) -> usize {
        todo!()
    }

    /// Phase 1 — whether the blob is empty.
    ///
    /// # Caveats
    ///
    /// Exists alongside [`Blob::len`] to satisfy `clippy::len_without_is_empty`, but it is
    /// genuinely meaningful here: empty files are tracked in Git (they are not directories) and
    /// `git add` of an empty file is a real, common case that must produce the empty blob rather
    /// than skipping the path.
    pub const fn is_empty(&self) -> bool {
        todo!()
    }

    /// Phase 1 — the blob's contents.
    ///
    /// # Caveats
    ///
    /// Same slice as the public field; this accessor exists so that generic code over the four
    /// object types can get at the payload without the field being part of that generic story.
    pub const fn data(&self) -> &'a [u8] {
        todo!()
    }

    /// Phase 1 — the blob's id under a given object format.
    ///
    /// # Caveats
    ///
    /// Hashes `"blob <len>\0"` followed by the data. Note the format is a parameter, not a
    /// property of the blob: the same bytes in a SHA-1 and a SHA-256 repository have different
    /// ids, and a `Blob` carries no repository context to know which it belongs to.
    ///
    /// # Errors
    ///
    /// Only the hasher's collision-detection path.
    pub fn id(&self, format: ObjectFormat) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 1 — serialise to the exact bytes that are stored and hashed.
    ///
    /// # Caveats
    ///
    /// Returns `"blob <len>\0"` + data, i.e. header **and** body — this is the buffer that gets
    /// zlib-compressed into `.git/objects/`, not the body alone. The distinction matters at the
    /// object database boundary: [`crate::object::hash_object`] returns the same buffer shape,
    /// and mixing the two conventions produces objects that hash correctly but fail to
    /// decompress to the declared length.
    pub fn to_bytes(&self) -> Vec<u8> {
        todo!()
    }

    /// Phase 1 — hash a blob without ever holding it in memory.
    ///
    /// The path for `hash-object` on a large file: the length is not known in advance, so the
    /// header cannot be written until the body has been consumed. The usual resolution is to
    /// stream into a temporary file while counting, then replay the header and the file through
    /// the hasher — or to require a seekable reader and get the length from its metadata.
    ///
    /// # Caveats
    ///
    /// `len` is supplied by the caller rather than derived, because a stream cannot tell you its
    /// own length. It is the caller's job to pass the true value; a wrong `len` yields a
    /// perfectly self-consistent file that Git will reject as a size mismatch, which is a
    /// confusing failure mode to debug from the other side.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Io`] on a read failure, plus the hasher's collision path.
    pub fn from_reader(
        format: ObjectFormat,
        reader: &mut dyn std::io::Read,
        len: u64,
    ) -> Result<(ObjectId, Vec<u8>)> {
        todo!()
    }

    /// Phase 1 — whether the contents look binary, using Git's heuristic.
    ///
    /// Git scans the first 8000 bytes for a NUL byte and, if it finds one, reports the file as
    /// binary in diffs. It is a heuristic and it is wrong in both directions (a UTF-16 text file
    /// is "binary"; a binary file whose first 8 KB happen to be NUL-free is "text"), but it is
    /// what `git diff` does, and matching it byte-for-byte is what the differential tests
    /// require.
    ///
    /// # Caveats
    ///
    /// The window is 8000 bytes, not 8192 — a detail that shows up immediately in a differential
    /// test against `git`. This is used by `diff` and by `status`'s binary handling; it must not
    /// be used to decide whether a file is *storable*, because every byte sequence is.
    pub fn is_binary(&self) -> bool {
        todo!()
    }
}
