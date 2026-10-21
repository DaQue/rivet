//! Phase 1 — the four Git object kinds, their parse and serialize, and loose-object storage.
//!
//! Git stores every object as
//!
//! ```text
//! <type> <length>\0<body>
//! ```
//!
//! and names it by the hash of exactly those bytes. The `<length>` is the length of the
//! **body** in bytes, not including the header, and not including the NUL. Getting it wrong
//! produces a file that Git reads happily but that hashes to a different id — the object is
//! then unfindable, and `git fsck` reports a hash/size mismatch.
//!
//! # Borrowed objects, owned transactions
//!
//! [`Blob`], [`Tree`], [`Commit`] and [`Tag`] **borrow their payload** with a lifetime
//! parameter, so parsing a 50 MB blob does not copy it. The types that must be edited and
//! accumulated — the index, the config, a pack index, and [`Object`]/[`AnyObject`] when used
//! as a transaction — own their data. Choose accordingly: pass `Blob<'a>` around, store
//! `Object` when you need to keep it.
//!
//! # Never panic on repository data
//!
//! Every parser here is total: it returns [`Result`] with a specific [`Error`] variant and
//! does not index, unwrap or slice without bounds checks (plan §5). Objects come from the
//! network and from other people's repositories, and this is the first code to touch them.

use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};
use bstr::BString;

pub mod blob;
pub mod commit;
pub mod tag;
pub mod tree;

// NOTE: `EntryMode` is defined in this file, not in `tree`, so it is not re-exported from there.
pub use blob::Blob;
pub use commit::{Commit, CommitBuilder, Signature};
pub use tag::{Tag, TagBuilder};
pub use tree::{Tree, TreeEntry, TreeEntryRef};

/// Phase 1 — which of Git's four object types an object is.
///
/// # Caveats
///
/// The `Display`/`as_bytes` spelling is the exact lowercase word that appears in the object
/// header and in `cat-file -t` output, so it is part of the on-disk and CLI contract, not a
/// cosmetic label.
///
/// The order of the variants is **not** significant. Pack code that needs a canonical order
/// must define its own, because Git's pack layout does not order objects by type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    /// A file's contents. Also used for symlinks, whose body is the link target.
    Blob,
    /// A directory listing: names, modes and ids, in canonical sorted order.
    Tree,
    /// A snapshot: one tree plus zero or more parents, author, committer and a message.
    Commit,
    /// An annotated tag: a named pointer to another object, with a tagger and a message.
    Tag,
}

impl ObjectKind {
    /// Phase 1 — the lowercase keyword for this kind, as it appears in an object header.
    ///
    /// # Caveats
    ///
    /// Must be byte-identical to what Git writes: `"blob"`, `"tree"`, `"commit"`, `"tag"`. The
    /// header is hashed, so a capital letter would silently change every object id in the
    /// repository.
    pub const fn as_bytes(self) -> &'static [u8] {
        match self {
            ObjectKind::Blob => b"blob",
            ObjectKind::Tree => b"tree",
            ObjectKind::Commit => b"commit",
            ObjectKind::Tag => b"tag",
        }
    }

    /// Phase 1 — the same spelling as a string slice, for CLI output and errors.
    pub const fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
            ObjectKind::Tag => "tag",
        }
    }

    /// Phase 1 — parse a type keyword from an object header or a `cat-file -t` argument.
    ///
    /// # Caveats
    ///
    /// Matching is byte-exact and lowercase; `"BLOB"` is not a valid kind. Git, however, also
    /// accepts some command-line aliases that are *not* object kinds (`git cat-file -t` on a
    /// peeled tag, `^{commit}` syntax). Those are resolved in revision parsing, not here.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedObject`] for an unrecognised or non-UTF-8 keyword. Note this is
    /// reported against a kind that has not been established yet, so the variant's `kind` field
    /// is the closest guess available; do not match on it to recover the real kind.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        match bytes {
            b"blob" => Ok(ObjectKind::Blob),
            b"tree" => Ok(ObjectKind::Tree),
            b"commit" => Ok(ObjectKind::Commit),
            b"tag" => Ok(ObjectKind::Tag),
            _ => Err(crate::error::Error::MalformedObject {
                kind: ObjectKind::Blob,
                reason: format!("unrecognised object kind {bytes:?}"),
            }),
        }
    }
}

/// Phase 1 — compute the id of an object without writing it.
///
/// Hashes `"<kind> <len>\0"` followed by `data` and returns both the id and those exact bytes,
/// so a caller that decides to persist the object can hand the buffer straight to the object
/// database without re-serialising. `hash-object -w` is the only CLI caller that wants the
/// writing variant; plain `hash-object` uses this and discards the buffer.
///
/// # Caveats
///
/// This is the **canonical** definition of an object's identity, and it must be the only one.
/// Any code that hashes an object by a different route — for instance by re-serialising a
/// parsed value instead of hashing the bytes it read — will disagree with Git on objects that
/// round-trip lossily, such as commits with an unusual header order.
///
/// # Errors
///
/// Only the collision-detection path of the hasher (see [`crate::hash::ObjectHasher::finish`]);
/// the `format` is always known-good here, unlike in id parsing.
pub fn hash_object(
    format: ObjectFormat,
    kind: ObjectKind,
    data: &[u8],
) -> Result<(ObjectId, Vec<u8>)> {
    // Build the buffer Git stores: "<kind> <len>\0" + body, where <len> is the
    // body length — never the whole buffer.
    let mut buf = Vec::with_capacity(kind.as_bytes().len() + 1 + data.len());
    buf.extend_from_slice(kind.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(data.len().to_string().as_bytes());
    buf.push(0);
    buf.extend_from_slice(data);

    let mut hasher = format.hasher();
    hasher.update(&buf);
    let id = hasher.finish()?;
    Ok((id, buf))
}

/// Phase 1 — an owned reference to one object of any kind.
///
/// This is the transaction form of an object: it owns its payload so it can be held, moved
/// into a struct or returned from a function that has already dropped the buffer it was parsed
/// from. Use [`Blob`], [`Tree`], [`Commit`] or [`Tag`] directly when borrowing is enough —
/// they cost no allocation.
///
/// # Caveats
///
/// The variant's payload is the object **body only**, with no `<type> <len>\0` header. Code
/// that writes an `Object` to the object database must prepend the header itself; a body
/// written without one will hash to a different id than the `ObjectId` it is filed under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    /// A blob body.
    Blob(Vec<u8>),
    /// A serialised tree body. Parsing it on demand is what [`AnyObject`] is for.
    Tree(Vec<u8>),
    /// A serialised commit body.
    Commit(Vec<u8>),
    /// A serialised tag body.
    Tag(Vec<u8>),
}

impl Object {
    /// Phase 1 — which kind this object is.
    ///
    /// # Caveats
    ///
    /// Answered from the variant, so it is always consistent with the payload — but note it
    /// carries no information about whether the payload is *well-formed* for that kind. Only
    /// [`AnyObject::parse`] checks that.
    pub const fn kind(&self) -> ObjectKind {
        match self {
            Object::Blob(_) => ObjectKind::Blob,
            Object::Tree(_) => ObjectKind::Tree,
            Object::Commit(_) => ObjectKind::Commit,
            Object::Tag(_) => ObjectKind::Tag,
        }
    }

    /// Phase 1 — the object body, without its header.
    pub fn data(&self) -> &[u8] {
        match self {
            Object::Blob(d) | Object::Tree(d) | Object::Commit(d) | Object::Tag(d) => d,
        }
    }

    /// Phase 1 — take ownership of the body, discarding the kind tag.
    pub fn into_data(self) -> Vec<u8> {
        match self {
            Object::Blob(d) | Object::Tree(d) | Object::Commit(d) | Object::Tag(d) => d,
        }
    }

    /// Phase 1 — the id this object would have, computed from its payload.
    ///
    /// # Caveats
    ///
    /// Computed, not stored — the `Object` has no id field, so this rehashes. A caller that
    /// already knows the id (from a ref, an index entry or a pack entry) should use that value
    /// and verify it once, rather than calling this in a loop.
    ///
    /// # Errors
    ///
    /// Only hasher collision detection.
    pub fn id(&self, format: ObjectFormat) -> Result<ObjectId> {
        hash_object(format, self.kind(), self.data()).map(|(id, _)| id)
    }

    /// Phase 1 — what this object is written as on disk and on the wire.
    ///
    /// # Caveats
    ///
    /// Names the object type for a reader that is about to decide how to decode it. For a
    /// [`ObjectKind::Blob`] there is nothing more to say; for a tree, commit or tag the caller
    /// must parse the returned bytes with the matching parser.
    ///
    /// Note that a `cat-file -t` argument of `blob` is *not* the same question — that asks what
    /// an object already in the database contains, which may differ from what the caller
    /// assumed.
    pub fn parse(&self) -> Result<AnyObject<'_>> {
        // An `Object` does not record the repository's hash, so it is read off the bytes: the
        // first header of a commit or tag carries a hex id of the object's width, and a tree of
        // the wrong width does not parse. Callers that know the format use [`Object::parse_as`].
        let header_width = |data: &[u8]| -> ObjectFormat {
            let line = data.split(|b| *b == b'\n').next().unwrap_or(&[]);
            let hex = line.iter().skip_while(|b| **b != b' ').skip(1).count();
            if hex == ObjectFormat::Sha256.hex_len() {
                ObjectFormat::Sha256
            } else {
                ObjectFormat::Sha1
            }
        };
        match self {
            Object::Blob(d) => Ok(AnyObject::Blob(blob::Blob { data: d })),
            Object::Tree(d) => match tree::Tree::parse(ObjectFormat::Sha1, d) {
                Ok(tree) => Ok(AnyObject::Tree(tree)),
                Err(first) => tree::Tree::parse(ObjectFormat::Sha256, d)
                    .map(AnyObject::Tree)
                    .map_err(|_| first),
            },
            Object::Commit(d) => Ok(AnyObject::Commit(commit::Commit::parse(
                header_width(d),
                d,
            )?)),
            Object::Tag(d) => Ok(AnyObject::Tag(tag::Tag::parse(header_width(d), d)?)),
        }
    }

    /// Parse as an object of a known repository hash.
    pub fn parse_as(&self, format: ObjectFormat) -> Result<AnyObject<'_>> {
        match self {
            Object::Blob(d) => Ok(AnyObject::Blob(blob::Blob { data: d })),
            Object::Tree(d) => Ok(AnyObject::Tree(tree::Tree::parse(format, d)?)),
            Object::Commit(d) => Ok(AnyObject::Commit(commit::Commit::parse(format, d)?)),
            Object::Tag(d) => Ok(AnyObject::Tag(tag::Tag::parse(format, d)?)),
        }
    }
}

/// Phase 1 — a borrowed, parsed view of an object, preserving its kind.
///
/// The borrowing counterpart of [`Object`]: parsing is free, and the variants keep their
/// lifetime so a caller can hold a `Blob<'a>` pointing straight into an mmap'd pack entry or a
/// decompressed loose-object buffer.
///
/// # Caveats
///
/// The lifetime is tied to the buffer the bytes came from, not to the object database. Anything
/// that needs to outlive that buffer — a cache, a queue of objects to send — must convert to
/// [`Object`] first, by copying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnyObject<'a> {
    /// A parsed blob.
    Blob(Blob<'a>),
    /// A parsed tree.
    Tree(Tree<'a>),
    /// A parsed commit.
    Commit(Commit<'a>),
    /// A parsed tag.
    Tag(Tag<'a>),
}

impl<'a> AnyObject<'a> {
    /// Phase 1 — parse an object body of a known kind.
    ///
    /// # Caveats
    ///
    /// `data` is the body only — the caller has already consumed the `<type> <len>\0` header,
    /// usually while validating the declared length against the actual one. Passing header and
    /// body together is the classic way to get a "malformed tree" that looks like corruption.
    ///
    /// The declared type in the header is authoritative but untrusted: a loose object whose
    /// filename says one thing and whose header says another is corrupt, and the object
    /// database, not this function, decides which to believe. Here the caller has already
    /// chosen, and `kind` is that choice.
    ///
    /// # Errors
    ///
    /// Whatever the per-kind parser raises — [`Error::MalformedObject`] for a body that does
    /// not follow the grammar, [`Error::DuplicateTreeEntry`], [`Error::InvalidEntryMode`] and
    /// friends for a malformed tree, [`Error::MissingHeader`] for a commit or tag that lacks a
    /// required header.
    pub fn parse(kind: ObjectKind, data: &'a [u8]) -> Result<Self> {
        match kind {
            ObjectKind::Blob => Ok(Self::Blob(Blob { data })),
            ObjectKind::Tree => Ok(Self::Tree(tree::Tree::parse(ObjectFormat::Sha1, data)?)),
            ObjectKind::Commit => Ok(Self::Commit(commit::Commit::parse(
                ObjectFormat::Sha1,
                data,
            )?)),
            ObjectKind::Tag => Ok(Self::Tag(tag::Tag::parse(ObjectFormat::Sha1, data)?)),
        }
    }

    /// Phase 1 — which kind this is.
    pub const fn kind(&self) -> ObjectKind {
        match self {
            Self::Blob(_) => ObjectKind::Blob,
            Self::Tree(_) => ObjectKind::Tree,
            Self::Commit(_) => ObjectKind::Commit,
            Self::Tag(_) => ObjectKind::Tag,
        }
    }

    /// Phase 1 — the raw body, without re-serialising it.
    ///
    /// # Caveats
    ///
    /// Returns the bytes that were parsed, not a fresh serialisation. That distinction matters
    /// for objects Rivet does not model exactly — a commit with an unknown extra header, say —
    /// where re-serialising could reorder headers and change the object's id. Hashing and
    /// sending must use these bytes, never a rebuild.
    pub fn data(&self) -> &'a [u8] {
        match self {
            Self::Blob(b) => b.data,
            Self::Tree(t) => t.data,
            Self::Commit(c) => c.raw,
            Self::Tag(t) => t.raw,
        }
    }

    /// Phase 1 — copy into an owned [`Object`], so it can outlive the source buffer.
    pub fn into_owned(self) -> Object {
        match self {
            AnyObject::Blob(b) => Object::Blob(b.data.to_vec()),
            AnyObject::Tree(t) => Object::Tree(t.data.to_vec()),
            AnyObject::Commit(c) => Object::Commit(c.raw.to_vec()),
            AnyObject::Tag(t) => Object::Tag(t.raw.to_vec()),
        }
    }
}

/// Phase 1 — the file mode of a tree entry, as Git's five legal values.
///
/// # Caveats
///
/// Git stores modes as **ASCII octal** in the tree body — `"100644"`, not the integer
/// `0o100644` — and the canonical serialisation has **no leading zero** for the `40000` case:
/// a directory is written `"40000"`, not `"040000"`. Reading is more forgiving than writing:
/// some tools emit `"040000"`, so a parser must accept a leading zero, while a serializer must
/// never produce one, because the extra byte changes the tree's hash.
///
/// Modes are not arbitrary POSIX modes. Only the file-type bits and the executable bit are
/// meaningful; Git normalises `100664` to `100644` and refuses anything that is not one of
/// these five. Rivet must do the same on `add` — a work tree file with an odd permission set
/// (a `setgid` bit, a sticky bit) is stored as plain `100644`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryMode {
    /// `100644` — a regular, non-executable file.
    Blob,
    /// `100755` — a regular, executable file.
    BlobExecutable,
    /// `120000` — a symlink, whose blob body is the link target as bytes.
    Link,
    /// `040000` — a subdirectory. Serialised in trees as `40000`.
    Tree,
    /// `160000` — a commit id recorded as a submodule ("gitlink"). Rivet does not implement
    /// submodules (plan §1), but must preserve these entries across checkout and commit so a
    /// repository that has them is not silently corrupted.
    Commit,
}

impl EntryMode {
    /// Phase 1 — the mode's numeric value, e.g. `0o100644`.
    ///
    /// # Caveats
    ///
    /// The number, not the octal text. Formatting it with `{:06o}` yields the on-disk spelling
    /// for four of the five values and the **wrong** one for [`EntryMode::Tree`], which must
    /// serialise as `40000` with five digits. Use [`EntryMode::write_bytes`] rather than
    /// formatting by hand.
    pub const fn to_u32(self) -> u32 {
        match self {
            EntryMode::BlobExecutable => 0o100755,
            EntryMode::Blob => 0o100644,
            EntryMode::Tree => 0o40000,
            EntryMode::Link => 0o120000,
            EntryMode::Commit => 0o160000,
        }
    }

    /// Phase 1 — parse a mode from its octal ASCII bytes, as found in a tree body.
    ///
    /// # Caveats
    ///
    /// Accepts an optional leading zero (`040000` as well as `40000`), because both appear in
    /// the wild, but normalises to the same [`EntryMode`]. Rejects `100664` and other
    /// non-canonical permissions rather than normalising them: a tree Rivet reads should be one
    /// Git wrote, and silently accepting a mode Rivet would never write hides corruption that
    /// `git fsck` would report.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidEntryMode`] if the digits are not a number, overflow, or name no legal
    /// Git mode. The error carries the raw value so the caller can report it in octal.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > 6 {
            return Err(Error::InvalidEntryMode {
                name: BString::default(),
                mode: 0,
            });
        }
        for &b in bytes {
            if !b.is_ascii_digit() {
                return Err(Error::InvalidEntryMode {
                    name: BString::default(),
                    mode: 0,
                });
            }
        }
        let value = bytes
            .iter()
            .fold(0u32, |acc, &b| acc * 8 + (b - b'0') as u32);
        Ok(match value {
            0o100755 => EntryMode::BlobExecutable,
            0o100644 => EntryMode::Blob,
            0o40000 => EntryMode::Tree,
            0o120000 => EntryMode::Link,
            0o160000 => EntryMode::Commit,
            _ => {
                return Err(Error::InvalidEntryMode {
                    name: BString::default(),
                    mode: value,
                });
            }
        })
    }

    /// Phase 1 — write the canonical octal ASCII form into `out`.
    ///
    /// # Caveats
    ///
    /// Writes the minimal number of digits: `100644`, `100755`, `120000`, `40000`, `160000`.
    /// The caller supplies the buffer because trees are serialised in tight loops; six bytes is
    /// always enough, and [`EntryMode::serialized_len`] reports the exact count.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedObject`] if `out` cannot hold the digits. This is a caller bug rather
    /// than bad repository data, but it is still an error and not a panic, and it cannot
    /// truncate — a truncated mode would produce a valid-looking tree with a different hash.
    pub fn write_bytes(self, out: &mut [u8]) -> Result<()> {
        let s = self.to_string();
        if out.len() < s.len() {
            return Err(Error::MalformedObject {
                kind: ObjectKind::Tree,
                reason: "output buffer too short".to_string(),
            });
        }
        out[..s.len()].copy_from_slice(s.as_bytes());
        Ok(())
    }

    /// Phase 1 — the canonical octal string form of this mode.
    pub const fn to_string(self) -> &'static str {
        match self {
            EntryMode::BlobExecutable => "100755",
            EntryMode::Blob => "100644",
            EntryMode::Tree => "40000",
            EntryMode::Link => "120000",
            EntryMode::Commit => "160000",
        }
    }

    /// Phase 1 — how many bytes [`EntryMode::write_bytes`] will write for this mode.
    ///
    /// # Caveats
    ///
    /// Five for a tree (`40000`), six for everything else — size a buffer with this rather than
    /// assuming six, or a tree entry will be padded and the tree will not match what Git wrote.
    pub const fn serialized_len(self) -> usize {
        match self {
            EntryMode::Tree => 5,
            _ => 6,
        }
    }

    /// Phase 1 — whether this mode can hold a nested tree.
    ///
    /// # Caveats
    ///
    /// True only for [`EntryMode::Tree`]. In particular this is **not** "is a directory" in any
    /// filesystem sense — [`EntryMode::Commit`] (a gitlink) looks like a directory to a
    /// checkout but is never descended into, and must not be reported as one.
    pub const fn is_tree(self) -> bool {
        matches!(self, EntryMode::Tree)
    }

    /// Phase 1 — the POSIX mode bits to apply when writing this entry to the work tree.
    ///
    /// # Caveats
    ///
    /// Lossy in both directions and deliberately so: Git stores only "executable or not" for
    /// regular files, so a checkout cannot restore the original permissions, and the result is
    /// `0o644`/`0o755` regardless of umask. Symlinks and gitlinks are not files to be mode-set
    /// and return `None`.
    pub const fn to_file_mode(self) -> Option<u32> {
        match self {
            EntryMode::BlobExecutable => Some(0o755),
            EntryMode::Blob => Some(0o644),
            _ => None,
        }
    }

    /// Phase 1 — classify a work tree file's mode as one of the stored modes.
    ///
    /// # Caveats
    ///
    /// This is where `core.filemode` becomes relevant: on a filesystem that does not report the
    /// executable bit, or with `core.filemode = false`, every regular file must classify as
    /// [`EntryMode::Blob`] even if the stat says otherwise, or `rivet status` reports spurious
    /// mode changes. On Windows, `core.symlinks = false` means links were checked out as plain
    /// files and must classify as blobs.
    pub const fn from_file_mode(mode: u32) -> Self {
        if mode & 0o111 != 0 {
            EntryMode::BlobExecutable
        } else {
            EntryMode::Blob
        }
    }
}

impl std::fmt::Display for EntryMode {
    /// Phase 1 — the canonical octal spelling, identical to what [`EntryMode::write_bytes`]
    /// produces. Note that a tree prints as `40000`, not `040000`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EntryMode::BlobExecutable => "100755",
            EntryMode::Blob => "100644",
            EntryMode::Tree => "40000",
            EntryMode::Link => "120000",
            EntryMode::Commit => "160000",
        })
    }
}

impl std::fmt::Display for ObjectKind {
    /// Phase 1 — the kind keyword, identical to [`ObjectKind::as_str`].
    ///
    /// # Caveats
    ///
    /// This impl exists because [`crate::error::Error`] embeds a kind in several of its
    /// messages (`"expected a blob object, found a commit"`), so every kind has to render. It
    /// is the on-disk keyword, not a pretty label — do not be tempted to capitalise it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::EntryMode;
    use crate::error::Error;

    #[test]
    fn from_bytes_canonical_modes() {
        assert_eq!(EntryMode::from_bytes(b"100644").unwrap(), EntryMode::Blob);
        assert_eq!(
            EntryMode::from_bytes(b"100755").unwrap(),
            EntryMode::BlobExecutable
        );
        assert_eq!(EntryMode::from_bytes(b"120000").unwrap(), EntryMode::Link);
        assert_eq!(EntryMode::from_bytes(b"40000").unwrap(), EntryMode::Tree);
        assert_eq!(EntryMode::from_bytes(b"160000").unwrap(), EntryMode::Commit);
    }

    #[test]
    fn from_bytes_leading_zero_normalises() {
        assert_eq!(EntryMode::from_bytes(b"040000").unwrap(), EntryMode::Tree);
    }

    #[test]
    fn from_bytes_rejects_non_canonical_permissions() {
        assert!(matches!(
            EntryMode::from_bytes(b"100664"),
            Err(Error::InvalidEntryMode { .. })
        ));
    }

    #[test]
    fn from_bytes_round_trip() {
        for m in [
            EntryMode::BlobExecutable,
            EntryMode::Blob,
            EntryMode::Tree,
            EntryMode::Link,
            EntryMode::Commit,
        ] {
            assert_eq!(EntryMode::from_bytes(m.to_string().as_bytes()).unwrap(), m);
        }
    }
}
