//! Phase 1 — object formats, object ids and the hashing abstraction.
//!
//! Git names every object by the hash of its serialised form, prefixed with a header:
//!
//! ```text
//! sha1("<type> <len>\0<body>")
//! ```
//!
//! Rivet uses **collision-detecting SHA-1** (`sha1-checked`) exactly as Git does, and plain
//! SHA-256 for SHA-256 repositories. SHA-1 is the default, because GitHub only accepts SHA-1
//! publicly today (plan §2.1); SHA-256 is supported end to end.
//!
//! # The one rule
//!
//! **Nothing outside this module may hard-code 20 bytes or 40 hex characters.** Objects,
//! trees, the index, packs, `.idx` files and pkt-lines all contain object ids, and all of them
//! must go through [`ObjectId`] and [`ObjectFormat`]. A `[u8; 20]` anywhere else is a bug that
//! will not show up until someone opens a SHA-256 repository.

use std::fmt;

use bstr::{BStr, BString, ByteSlice};
use sha1_checked::Sha1;
use sha2::{Digest, Sha256};

// `Error` is referenced only from doc comments in this module, and a doc-link import trips
// `unused_imports`. Later batches will need it for real once the bodies exist.
use crate::error::Result;

/// Phase 1 — which hash algorithm a repository uses.
///
/// This is fixed when the repository is created and can never change: it is recorded in
/// `.git/config` as `extensions.objectFormat`, alongside
/// `core.repositoryformatversion = 1`.
///
/// # Caveats
///
/// Git's *default* is `sha1`, and so is Rivet's. A SHA-256 repository is **not** a SHA-1
/// repository with longer ids: every id — in trees, in the index, in packs, in refs, on the
/// wire — is the other width, and the two histories cannot be exchanged at all (plan §2.1
/// rule 5; Git has not finished that interop work either).
///
/// `Sha1` here means Git's *collision-detecting* SHA-1, which hashes a counter as well as the
/// object. It produces the same digests as plain SHA-1 for non-colliding input, so existing
/// repositories still work — but do not reach for `sha1` when implementing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectFormat {
    /// SHA-1 — 20 bytes, 40 hex characters. The default, and the only format GitHub accepts.
    Sha1,
    /// SHA-256 — 32 bytes, 64 hex characters. Supported by GitLab, Codeberg, Forgejo, Gitea.
    Sha256,
}

impl ObjectFormat {
    /// Phase 1 — the number of raw bytes in an id of this format.
    ///
    /// # Caveats
    ///
    /// 20 or 32. This is the width of the *stored* id, not of a hex string — see
    /// [`ObjectFormat::hex_len`] for that, which is exactly twice this.
    #[allow(clippy::len_without_is_empty)] // an id is never empty
    pub const fn len(self) -> usize {
        match self {
            ObjectFormat::Sha1 => 20,
            ObjectFormat::Sha256 => 32,
        }
    }

    /// Phase 1 — the number of hex characters in a full id of this format.
    ///
    /// # Caveats
    ///
    /// Always `2 * len()`. Callers frequently want "40 or 64" as a validation bound; derive it
    /// from here rather than writing the numbers out.
    pub const fn hex_len(self) -> usize {
        2 * self.len()
    }

    /// Phase 1 — build a hasher for this format.
    ///
    /// # Caveats
    ///
    /// Returns a boxed trait object, so a hasher can be held as `Box<dyn ObjectHasher>`
    /// whatever the format. The cost is one virtual call per `update`, which is irrelevant
    /// next to the compression and I/O in the same code paths.
    pub fn hasher(self) -> Box<dyn ObjectHasher> {
        match self {
            ObjectFormat::Sha1 => Box::new(Sha1::builder().build()),
            ObjectFormat::Sha256 => Box::new(Sha256::new()),
        }
    }

    /// Phase 1 — the spelling Git uses in config: `"sha1"` or `"sha256"`.
    ///
    /// # Caveats
    ///
    /// This is the value of `extensions.objectFormat`. It is written verbatim to `.git/config`
    /// and compared against what a remote announces during the protocol v2 handshake, so the
    /// spelling is part of the wire format and cannot be changed for cosmetics.
    pub const fn as_str(self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }

    /// Phase 1 — parse the `extensions.objectFormat` config value.
    ///
    /// # Caveats
    ///
    /// Matching is case-insensitive, because Rust's `String`/`&str` comparison and Git's
    /// `strcasecmp` differ here and Git accepts both spellings.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::UnknownObjectFormat`] for anything other than `sha1` or `sha256` — including
    /// forward-compatible values a newer Git might write. Guessing would produce a repository
    /// Rivet cannot read, so this refuses.
    pub fn from_config_value(value: &BStr) -> Result<Self> {
        match value.as_bytes().to_ascii_lowercase().as_slice() {
            b"sha1" => Ok(ObjectFormat::Sha1),
            b"sha256" => Ok(ObjectFormat::Sha256),
            _ => Err(crate::error::Error::UnknownObjectFormat(BString::from(
                value,
            ))),
        }
    }
}

impl fmt::Display for ObjectFormat {
    /// Phase 1 — print the config spelling, identical to [`ObjectFormat::as_str`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Phase 1 — the identity of one object: a hash, at one of two widths.
///
/// # Caveats
///
/// This is an enum rather than a fixed-size array so that the width is carried in the type and
/// a SHA-1 id can never be silently compared against a SHA-256 one — the derived `PartialEq`
/// returns `false` for cross-format pairs. Do not add a `From<[u8; 20]>`-style widening
/// conversion: mixing formats is always a bug.
///
/// The ordering is derived from the enum discriminant first, so **all SHA-1 ids sort before
/// all SHA-256 ids**. Within a single repository that never matters, because all ids have the
/// same format; do not rely on the ordering being meaningful across formats.
///
/// `Display` prints the full hex id, and is what ref files, reflogs, `.idx` files and the wire
/// protocol ultimately agree on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectId {
    /// A 20-byte SHA-1 id.
    Sha1([u8; 20]),
    /// A 32-byte SHA-256 id.
    Sha256([u8; 32]),
}

impl ObjectId {
    /// Phase 1 — the raw digest bytes, without a heap allocation.
    ///
    /// # Caveats
    ///
    /// The returned slice borrows `self`, which is `Copy`, so the borrow is easy to outlive.
    /// Its length is 20 or 32 — use [`ObjectFormat::len`] rather than assuming either.
    pub const fn as_bytes(&self) -> &[u8] {
        match self {
            ObjectId::Sha1(bytes) => bytes,
            ObjectId::Sha256(bytes) => bytes,
        }
    }

    /// Phase 1 — which format this id belongs to.
    pub const fn format(self) -> ObjectFormat {
        match self {
            ObjectId::Sha1(_) => ObjectFormat::Sha1,
            ObjectId::Sha256(_) => ObjectFormat::Sha256,
        }
    }

    /// Phase 1 — whether the id is entirely zero bytes.
    ///
    /// # Caveats
    ///
    /// The all-zero id is Git's "null oid" — the `old` value in a push's ref update when the
    /// ref is being created, and the absent parent in some external formats. It is not a valid
    /// object id and can never name a stored object.
    pub const fn is_zero(&self) -> bool {
        let bytes = self.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i] == 0 {
            i += 1;
        }
        i == bytes.len()
    }

    /// Phase 1 — the all-zero id of the given format.
    ///
    /// # Caveats
    ///
    /// For protocol code that needs a `new`/`old` placeholder when a ref does not yet exist.
    /// Passing the result to the object database will produce [`crate::error::Error::ObjectNotFound`].
    pub const fn zero(format: ObjectFormat) -> Self {
        match format {
            ObjectFormat::Sha1 => ObjectId::Sha1([0u8; 20]),
            ObjectFormat::Sha256 => ObjectId::Sha256([0u8; 32]),
        }
    }

    /// Phase 1 — read an id from raw bytes of the given format.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidHash`] if `bytes.len()` does not equal `format.len()`. Note that a
    /// wrong-length slice is an error, never a truncation or a zero-extension: both would
    /// produce an id that silently addresses the wrong object.
    pub fn from_bytes(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let expected = format.len();
        if bytes.len() != expected {
            return Err(crate::error::Error::InvalidHash {
                expected_len: expected,
                found_len: bytes.len(),
            });
        }
        match format {
            ObjectFormat::Sha1 => Ok(ObjectId::Sha1(bytes.try_into().unwrap())),
            ObjectFormat::Sha256 => Ok(ObjectId::Sha256(bytes.try_into().unwrap())),
        }
    }

    /// Phase 1 — parse a **full-length** hex id.
    ///
    /// # Example
    ///
    /// ```
    /// use bstr::BStr;
    /// use rivet_core::{ObjectFormat, ObjectId};
    ///
    /// let id = ObjectId::from_hex(
    ///     ObjectFormat::Sha1,
    ///     BStr::new("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"),
    /// )?;
    /// assert_eq!(id.to_string(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    /// # Ok::<(), rivet_core::Error>(())
    /// ```
    ///
    /// # Caveats
    ///
    /// Accepts both cases and no separators; Git writes lowercase, so that is what serialisation
    /// must produce. This parses a complete id only — for an abbreviation use
    /// [`ObjectId::from_hex_prefix`] or, more usually, let the object database resolve it with
    /// [`crate::odb::Odb::resolve_prefix`].
    ///
    /// There is no way to know the format from the string alone in general, so the caller
    /// supplies it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidHex`] if the length is not `format.hex_len()`, or if any character is
    /// not a hex digit.
    pub fn from_hex(format: ObjectFormat, hex: &BStr) -> Result<Self> {
        let expected = format.hex_len();
        if hex.len() != expected {
            return Err(crate::error::Error::InvalidHex {
                value: BString::from(hex),
                reason: "expected full-length hex",
            });
        }
        let mut bytes = [0u8; 32];
        decode_hex(&mut bytes[..expected / 2], hex.as_bytes())?;
        match format {
            ObjectFormat::Sha1 => Ok(ObjectId::Sha1(bytes[..20].try_into().unwrap())),
            ObjectFormat::Sha256 => Ok(ObjectId::Sha256(bytes)),
        }
    }

    /// Phase 1 — parse a hex prefix of between 1 and `hex_len` characters.
    ///
    /// # Caveats
    ///
    /// The result carries meaning only as a **prefix**: it is a query, not an object id. Git's
    /// shortest-unique-prefix rule means 4 characters is the usual minimum, but this function
    /// does not enforce that — the object database does, when resolving.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidHex`] if the string is empty, longer than `format.hex_len()`, or
    /// contains a non-hex character.
    pub fn from_hex_prefix(format: ObjectFormat, hex: &BStr) -> Result<Self> {
        let expected = format.hex_len();
        if hex.is_empty() || hex.len() > expected {
            return Err(crate::error::Error::InvalidHex {
                value: BString::from(hex),
                reason: "prefix length out of range",
            });
        }
        let mut bytes = [0u8; 32];
        decode_hex(&mut bytes[..hex.len() / 2], hex.as_bytes())?;
        match format {
            ObjectFormat::Sha1 => Ok(ObjectId::Sha1(bytes[..20].try_into().unwrap())),
            ObjectFormat::Sha256 => Ok(ObjectId::Sha256(bytes)),
        }
    }

    /// Phase 1 — hex-decode into `out`, which must be exactly `format.hex_len()` bytes.
    ///
    /// # Caveats
    ///
    /// Writes lowercase. This is the hot path when serialising thousands of `.idx` entries, so
    /// it is given the caller's buffer rather than allocating one.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidHex`] if `out.len()` is not `self.format().hex_len()`.
    pub fn write_hex(&self, out: &mut [u8]) -> Result<()> {
        let expected = self.format().hex_len();
        if out.len() != expected {
            return Err(crate::error::Error::InvalidHex {
                value: BString::from(&out[..]),
                reason: "output buffer has wrong length",
            });
        }
        encode_hex(out, self.as_bytes());
        Ok(())
    }

    /// Phase 1 — the full lowercase hex id, heap-allocated.
    ///
    /// # Caveats
    ///
    /// Convenience for display and error messages. Performance-sensitive paths must build the
    /// hex in place with [`ObjectId::write_hex`] instead — the object database hashes and
    /// formats on every read, and this allocates.
    pub fn to_hex(&self) -> BString {
        let mut buf = vec![0u8; self.format().hex_len()];
        encode_hex(&mut buf, self.as_bytes());
        BString::from(buf)
    }

    /// Phase 1 — does this id begin with `prefix`?
    ///
    /// # Caveats
    ///
    /// This is the primitive under short-id resolution: an abbreviation matches an object when
    /// the object's id has that abbreviation as a prefix. It says nothing about uniqueness —
    /// caller-side disambiguation still has to count matches.
    ///
    /// Returns `false` when the formats differ.
    pub fn starts_with(&self, prefix: &ObjectId) -> bool {
        if self.format() != prefix.format() {
            return false;
        }
        // A prefix built by `from_hex_prefix` is left-aligned and zero-filled to the full
        // width, so its meaningful span ends at the last non-zero byte. Treating the fill as
        // unspecified is the only reading that makes this the prefix primitive it documents.
        let p = prefix.as_bytes();
        let span = p.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        self.as_bytes()[..span] == p[..span]
    }

    /// Phase 1 — read an id from the wire or from a file, as the format's raw bytes.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidHash`] if fewer than `format.len()` bytes remain.
    pub fn read(format: ObjectFormat, data: &[u8]) -> Result<Self> {
        if data.len() < format.len() {
            return Err(crate::error::Error::InvalidHash {
                expected_len: format.len(),
                found_len: data.len(),
            });
        }
        Self::from_bytes(format, &data[..format.len()])
    }
}

impl fmt::Display for ObjectId {
    /// Phase 1 — the full lowercase hex id, identical to [`ObjectId::to_hex`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; 64];
        encode_hex(&mut buf[..self.format().hex_len()], self.as_bytes());
        f.write_str(std::str::from_utf8(&buf[..self.format().hex_len()]).unwrap())
    }
}

/// Phase 1 — an incremental hash over something that is not going to sit in memory at once.
///
/// Git hashes the header and body as separate `update` calls; `hash-object` on a large file
/// streams the body through in chunks. The trait is `Debug` so that a
/// `Box<dyn ObjectHasher>` can be embedded in a struct that derives `Debug`.
///
/// # Caveats
///
/// The trait is **not** object-safe by accident: [`ObjectHasher::finish`] takes `&mut self` and
/// returns a value, which is fine, but `Clone` would not be. Do not add `Clone` or
/// `PartialEq` — hashers hold non-cloneable hash state.
///
/// For SHA-1 this is Git's collision-detecting variant. It is stateful in a way plain SHA-1 is
/// not, which is why there is no way to hand it a complete message and get a digest back in
/// one call.
pub trait ObjectHasher: fmt::Debug {
    /// Phase 1 — feed more bytes into the hash.
    ///
    /// # Caveats
    ///
    /// Order matters, and callers must remember the header: the digest is over
    /// `"<type> <len>\0"` followed by the body, not the body alone. Feeding the body without
    /// the header is the classic way to produce ids that look right and match nothing.
    fn update(&mut self, data: &[u8]);

    /// Phase 1 — finish the hash and return the object id.
    ///
    /// # Caveats
    ///
    /// Consumes the accumulated state: calling `update` afterwards is not meaningful for a
    /// `sha1-checked` hasher. For collision-detecting SHA-1, `finish` is also where a detected
    /// collision surfaces.
    ///
    /// # Errors
    ///
    /// The collision case for SHA-1. All other failures are impossible — a hash of bounded
    /// input cannot fail — but the signature is `Result` so that the collision path does not
    /// have to be a panic, and so that the call site does not change if the implementation
    /// needs to grow a failure mode.
    fn finish(&mut self) -> Result<ObjectId>;
}

impl ObjectHasher for Sha1 {
    fn update(&mut self, data: &[u8]) {
        <Sha1 as Digest>::update(self, data);
    }

    fn finish(&mut self) -> Result<ObjectId> {
        let result = self.clone().try_finalize();
        if result.has_collision() {
            return Err(crate::error::Error::Message(
                "sha1 collision detected while hashing an object".into(),
            ));
        }
        let mut digest = [0u8; 20];
        digest.copy_from_slice(&result.hash()[..]);
        Ok(ObjectId::from_bytes(ObjectFormat::Sha1, &digest).unwrap())
    }
}

impl ObjectHasher for Sha256 {
    fn update(&mut self, data: &[u8]) {
        <Sha256 as Digest>::update(self, data);
    }

    fn finish(&mut self) -> Result<ObjectId> {
        let out = <Sha256 as Digest>::finalize_reset(self);
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&out[..]);
        Ok(ObjectId::Sha256(digest))
    }
}

/// Decode `hex.len() / 2` pairs of hex digits from `hex` into `out`.
///
/// Returns [`crate::error::Error::InvalidHex`] with `"not a hex digit"` on the first
/// non-hex byte. The caller is responsible for checking `out.len()`.
fn decode_hex(out: &mut [u8], hex: &[u8]) -> Result<()> {
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bad = |reason: &'static str| crate::error::Error::InvalidHex {
        value: BString::from(hex),
        reason,
    };
    if !hex.len().is_multiple_of(2) {
        return Err(bad("odd number of hex digits"));
    }
    for (pair, out_byte) in hex.as_chunks::<2>().0.iter().zip(out.iter_mut()) {
        let hi = nibble(pair[0]).ok_or_else(|| bad("not a hex digit"))?;
        let lo = nibble(pair[1]).ok_or_else(|| bad("not a hex digit"))?;
        *out_byte = (hi << 4) | lo;
    }
    Ok(())
}

/// Encode `bytes` as lowercase hex into `out`, which must have exactly twice as many slots.
fn encode_hex(out: &mut [u8], bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (byte, pair) in bytes.iter().zip(out.as_chunks_mut::<2>().0.iter_mut()) {
        pair[0] = HEX[(byte >> 4) as usize];
        pair[1] = HEX[(byte & 0x0f) as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    /// The canonical Git answer: the empty blob, `sha1("blob 0\0")`.
    fn empty_blob_id(format: ObjectFormat) -> ObjectId {
        let mut hasher = format.hasher();
        let header = b"blob 0\0";
        hasher.update(header);
        hasher
            .finish()
            .expect("hashing the empty blob must not fail")
    }

    #[test]
    fn empty_blob_sha1_matches_git() {
        let id = empty_blob_id(ObjectFormat::Sha1);
        assert_eq!(
            id.to_hex(),
            b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
            "empty blob must hash to Git's well-known id"
        );
        assert_eq!(
            id,
            ObjectId::from_hex(
                ObjectFormat::Sha1,
                "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into()
            )
            .unwrap()
        );
    }

    #[test]
    fn empty_blob_sha256_matches_known_answer() {
        let id = empty_blob_id(ObjectFormat::Sha256);
        assert_eq!(
            id.to_hex(),
            b"473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813",
            "empty blob must hash to the SHA-256 of \"blob 0\\0\""
        );
    }

    #[test]
    fn sha1_object_header_changes_the_id() {
        // The same body under a different object kind must give a different id:
        // hashing "commit 0\0" is not hashing "blob 0\0".
        let mut hasher = ObjectFormat::Sha1.hasher();
        hasher.update(b"commit 0\0");
        let commit_id = hasher.finish().unwrap();
        assert_ne!(
            commit_id,
            ObjectId::Sha1([
                0xe6, 0x9d, 0xe2, 0x9b, 0xb2, 0xd1, 0xd6, 0x43, 0x4b, 0x8b, 0x29, 0xae, 0x77, 0x5a,
                0xd8, 0xc2, 0xe4, 0x8c, 0x53, 0x91
            ]),
            "header must be part of the digest"
        );
    }

    #[test]
    fn format_parse_display_round_trip() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let value = BStr::new(format.as_str().as_bytes());
            assert_eq!(ObjectFormat::from_config_value(value).unwrap(), format);
            assert_eq!(format.to_string(), format.as_str());
        }
    }

    #[test]
    fn format_from_config_value_case_insensitive_and_refuses_unknown() {
        assert_eq!(
            ObjectFormat::from_config_value("SHA1".into()).unwrap(),
            ObjectFormat::Sha1
        );
        assert_eq!(
            ObjectFormat::from_config_value("Sha256".into()).unwrap(),
            ObjectFormat::Sha256
        );
        match ObjectFormat::from_config_value("md5".into()) {
            Err(Error::UnknownObjectFormat(_)) => {}
            other => panic!("expected UnknownObjectFormat, got {other:?}"),
        }
    }

    #[test]
    fn object_id_hex_round_trip() {
        let bytes: &[u8; 20] = &[
            0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        let id = ObjectId::from_bytes(ObjectFormat::Sha1, bytes).unwrap();
        assert_eq!(
            id.to_hex(),
            b"deadbeef00112233445566778899aabbccddeeff",
            "hex encoding must be lowercase and byte-exact"
        );
        assert_eq!(id.format(), ObjectFormat::Sha1);
        assert_eq!(
            id,
            ObjectId::from_hex(
                ObjectFormat::Sha1,
                "DEADBEEF00112233445566778899AABBCCDDEEFF".into()
            )
            .unwrap()
        );

        let bytes: &[u8; 32] = &[
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        let id = ObjectId::from_bytes(ObjectFormat::Sha256, bytes).unwrap();
        assert_eq!(
            id.to_hex(),
            b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        );
        assert_eq!(
            id,
            ObjectId::from_hex(
                ObjectFormat::Sha256,
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".into()
            )
            .unwrap()
        );
    }

    #[test]
    fn object_id_write_hex_matches_display() {
        let id = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into(),
        )
        .unwrap();
        let mut buf = vec![0u8; ObjectFormat::Sha1.hex_len()];
        id.write_hex(&mut buf).unwrap();
        assert_eq!(buf, *b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(id.to_hex().as_bytes(), format!("{id}").as_bytes());
    }

    #[test]
    fn from_hex_rejects_bad_lengths_and_non_hex() {
        match ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c539".into(),
        ) {
            Err(Error::InvalidHex { .. }) => {}
            other => panic!("short id must be rejected, got {other:?}"),
        }
        match ObjectId::from_hex(
            ObjectFormat::Sha256,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into(),
        ) {
            Err(Error::InvalidHex { .. }) => {}
            other => panic!("40-digit id must be rejected for SHA-256, got {other:?}"),
        }
        match ObjectId::from_hex(
            ObjectFormat::Sha1,
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".into(),
        ) {
            Err(Error::InvalidHex { .. }) => {}
            other => panic!("non-hex must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn from_hex_prefix_bounds_and_reads_into_zero_filled_id() {
        let id = ObjectId::from_hex_prefix(ObjectFormat::Sha1, "e69de29b".into()).unwrap();
        // The prefix is left-aligned, zero-padded: it matches nothing else in a real repo
        // query, and the zero padding must not leak into the id's remaining bytes.
        assert_eq!(&id.as_bytes()[..4], b"\xe6\x9d\xe2\x9b");
        assert_eq!(&id.as_bytes()[4..], &[0u8; 16]);

        match ObjectId::from_hex_prefix(ObjectFormat::Sha1, "".into()) {
            Err(Error::InvalidHex { .. }) => {}
            other => panic!("empty prefix must be rejected, got {other:?}"),
        }
        match ObjectId::from_hex_prefix(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391x".into(),
        ) {
            Err(Error::InvalidHex { .. }) => {}
            other => panic!("overlong prefix must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn starts_with_matches_prefix_and_rejects_cross_format() {
        let full = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into(),
        )
        .unwrap();
        let prefix = ObjectId::from_hex_prefix(ObjectFormat::Sha1, "e69d".into()).unwrap();
        assert!(full.starts_with(&prefix));
        assert!(!prefix.starts_with(&full));

        let other = ObjectId::zero(ObjectFormat::Sha256);
        assert!(!full.starts_with(&other));
        assert!(!other.starts_with(&full));
    }

    #[test]
    fn zero_ids_and_is_zero() {
        let z1 = ObjectId::zero(ObjectFormat::Sha1);
        let z2 = ObjectId::zero(ObjectFormat::Sha256);
        assert!(z1.is_zero() && z2.is_zero());
        let non_zero = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into(),
        )
        .unwrap();
        assert!(!non_zero.is_zero());
        assert_eq!(z1.format(), ObjectFormat::Sha1);
        assert_eq!(z2.format(), ObjectFormat::Sha256);
    }

    #[test]
    fn from_bytes_rejects_wrong_length() {
        match ObjectId::from_bytes(ObjectFormat::Sha1, &[0u8; 19]) {
            Err(Error::InvalidHash { .. }) => {}
            other => panic!("19 bytes must be rejected for SHA-1, got {other:?}"),
        }
        match ObjectId::from_bytes(ObjectFormat::Sha1, &[0u8; 32]) {
            Err(Error::InvalidHash { .. }) => {}
            other => panic!("32 bytes must be rejected for SHA-1, got {other:?}"),
        }
    }

    #[test]
    fn read_consumes_prefix_from_buffer() {
        let payload: [u8; 24] = [
            0xe6, 0x9d, 0xe2, 0x9b, 0xb2, 0xd1, 0xd6, 0x43, 0x4b, 0x8b, 0x29, 0xae, 0x77, 0x5a,
            0xd8, 0xc2, 0xe4, 0x8c, 0x53, 0x91, 0x00, 0x01, 0x02, 0x03,
        ];
        let id = ObjectId::read(ObjectFormat::Sha1, &payload).unwrap();
        assert_eq!(id.to_hex(), b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");

        match ObjectId::read(ObjectFormat::Sha256, &payload) {
            Err(Error::InvalidHash { .. }) => {}
            other => panic!("24 bytes must be rejected for SHA-256, got {other:?}"),
        }
    }
}
