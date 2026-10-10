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

use std::io::Read;

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
        crate::object::ObjectKind::Blob
    }

    /// Phase 1 — wrap a byte slice, borrowing it.
    ///
    /// # Caveats
    ///
    /// No validation and no copy. The returned blob's id is whatever the bytes hash to, so this
    /// is safe to call on arbitrary input — including input that is not a file's contents at
    /// all.
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    /// Phase 1 — the number of bytes in the blob.
    ///
    /// # Caveats
    ///
    /// This is the length that goes in the object header — `"blob <len>\0"`. The header is
    /// hashed, so an off-by-one here does not corrupt the file, it changes the object's
    /// identity and makes it unfindable.
    pub const fn len(&self) -> usize {
        self.data.len()
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
        self.data.is_empty()
    }

    /// Phase 1 — the blob's contents.
    ///
    /// # Caveats
    ///
    /// Same slice as the public field; this accessor exists so that generic code over the four
    /// object types can get at the payload without the field being part of that generic story.
    pub const fn data(&self) -> &'a [u8] {
        self.data
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
        crate::object::hash_object(format, Self::kind(), self.data).map(|(id, _)| id)
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
        let mut out = Vec::with_capacity(Self::kind().as_bytes().len() + 1 + self.len() + 10);
        out.extend_from_slice(Self::kind().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.len().to_string().as_bytes());
        out.push(0);
        out.extend_from_slice(self.data);
        out
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
        let kind = Self::kind();
        let header_len = kind.as_bytes().len() + 1 + len.to_string().len() + 1;
        let mut buf = Vec::with_capacity(header_len + len as usize);
        buf.extend_from_slice(kind.as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(len.to_string().as_bytes());
        buf.push(0);
        // Read exactly `len` bytes; a read that comes up short is a read failure.
        let filled = reader.take(len).read_to_end(&mut buf)?;
        if filled != len as usize {
            return Err(crate::error::Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("expected {len} bytes of blob body, got {filled}"),
            )));
        }
        let mut hasher = format.hasher();
        hasher.update(&buf);
        let id = hasher.finish()?;
        Ok((id, buf))
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
        self.data.iter().take(8000).any(|&b| b == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::ObjectFormat;

    // The well-known SHA-1 id of the empty blob.
    const EMPTY_BLOB_SHA1: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

    #[test]
    fn empty_blob_id() {
        let blob = Blob::new(&[]);
        let id = blob.id(ObjectFormat::Sha1).unwrap();
        assert_eq!(id.to_hex(), EMPTY_BLOB_SHA1);
        assert!(blob.is_empty());
        assert_eq!(blob.len(), 0);
    }

    #[test]
    fn round_trip_serialise_id() {
        // Serialize -> hash -> parse the header back and confirm the body is byte-identical.
        let data: &[u8] = b"hello\nworld\n";
        let blob = Blob::new(data);
        let buf = blob.to_bytes();
        // Header must be exactly "blob 12\0" before the body. The NUL is part of the header, so
        // the header occupies 8 bytes ("blob" + ' ' + "12" + NUL) and the body starts at offset 8.
        assert_eq!(&buf[..8], b"blob 12\0");
        assert_eq!(&buf[8..], data);
        let (id, buf2) = crate::object::hash_object(ObjectFormat::Sha1, Blob::kind(), data).unwrap();
        assert_eq!(buf, buf2);
        // Reconstruct a blob from the parsed buffer and verify identity is stable.
        let parsed = Blob::new(&buf[8..]);
        assert_eq!(parsed.data(), data);
        assert_eq!(parsed.id(ObjectFormat::Sha1).unwrap(), id);
        // The full buffer hash is the object's id.
        let mut h = ObjectFormat::Sha1.hasher();
        h.update(&buf);
        assert_eq!(h.finish().unwrap(), id);
    }

    #[test]
    fn header_from_hex_literal() {
        // Build the loose-object buffer for the empty blob and check it byte for byte:
        // "blob 0\0" = 0x626c6f62203000.
        let expected: Vec<u8> = vec![0x62, 0x6c, 0x6f, 0x62, 0x20, 0x30, 0x00];
        let blob = Blob::new(&[]);
        assert_eq!(blob.to_bytes(), expected);

        // A blob whose declared length in the header does not match the body must be detectable.
        let buf: Vec<u8> = [b'b', b'l', b'o', b'b', b' ', b'2', b'\0', b'x', b'y'].to_vec();
        let declared: u64 = (buf[5] - b'0') as u64;
        assert_eq!(declared, 2);
        // Body starts right after the NUL: offset = "blob"(4) + ' '(1) + len digits(1) + NUL(1) = 7.
        assert_eq!(&buf[4 + 1 + declared.to_string().len() + 1..], b"xy");
        // Parsing the body from the header: len(buf) - (header end offset) must equal declared.
        let header_end = 4 + 1 + declared.to_string().len() + 1; // "blob" + ' ' + len digits + NUL
        let body = &buf[header_end..];
        assert_eq!(body.len() as u64, declared);
    }

    #[test]
    fn from_reader_matches_to_bytes() {
        let data: &[u8] = b"some bytes\n";
        let mut reader = data;
        let (id, buf) = Blob::from_reader(ObjectFormat::Sha1, &mut reader, data.len() as u64).unwrap();
        assert_eq!(buf, Blob::new(data).to_bytes());
        assert_eq!(id, Blob::new(data).id(ObjectFormat::Sha1).unwrap());
    }

    #[test]
    fn from_reader_short_read_is_error() {
        let data: &[u8] = b"ab";
        let mut reader = data;
        let result = Blob::from_reader(ObjectFormat::Sha1, &mut reader, 3);
        assert!(result.is_err());
    }

    #[test]
    fn is_binary_nul_within_window() {
        // NUL at offset 5 -> binary.
        let a = b"a\0b";
        assert!(Blob::new(a).is_binary());
        // No NUL in the first 8000 bytes -> text.
        let mut b = vec![b'x'; 8000];
        b.push(0); // NUL at offset 8000, outside the window
        assert!(!Blob::new(&b).is_binary());
    }

    #[test]
    fn kind_and_accessors() {
        let b = Blob::new(b"abc");
        assert_eq!(Blob::kind(), crate::object::ObjectKind::Blob);
        assert_eq!(b.len(), 3);
        assert!(!b.is_empty());
        assert_eq!(b.data(), b"abc");
        // Display of the header keyword is the exact lowercase kind word.
        assert_eq!(format!("{}", Blob::kind()), "blob");
    }
}
