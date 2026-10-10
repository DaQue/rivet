//! Phase 10 — loose objects: one zlib file per object, under `objects/xx/`.
//!
//! The original Git storage format, and still the one every write produces. An object with id
//! `a1b2c3…` lives at `objects/a1/b2c3…`, and the file is the zlib-compressed bytes
//!
//! ```text
//! <type> <length>\0<body>
//! ```
//!
//! whose SHA-1 (or SHA-256) is the id that names the file. The two-hex-directory split exists
//! because a flat directory with a million entries is unusable on the filesystems Git was written
//! for; the first byte of the id is a free 256-way fanout.
//!
//! # Caveats that bite
//!
//! * **The filename is a claim, the content is the truth.** Nothing in the file records its own id.
//!   A loose object read without rehashing is an object read on faith, and the failure it hides —
//!   a file whose name does not match its content — is exactly what a hash collision or a bad copy
//!   produces. Every read here rehashes and compares (plan §5).
//! * **The declared length and the actual body length must agree.** A short body with a longer
//!   declared length is corruption, not a partial read: the zlib stream carries its own end, so
//!   there is no "more to come" state. This check is what turns a truncated object into
//!   [`crate::error::Error::CorruptObject`] instead of an object with a plausible type and size.
//! * **Hex directories are lowercase.** Git writes `objects/a1/…`, never `objects/A1/…`, and looks
//!   them up case-sensitively where the filesystem is case-sensitive. A reader that normalised case
//!   would find objects on Windows and miss them on Linux.
//! * **Writes are temp-file-then-rename**, which is what makes a concurrent `git gc` see either
//!   nothing or a complete object. The temp name must be in the same directory as the target —
//!   `objects/tmp_obj_…` on another filesystem cannot be renamed atomically into place.
//! * **Loose objects are written read-only (mode 0444).** Git does this so that a corrupted object
//!   with a valid name is not silently rewritten by the next process to read it. Rivet must match
//!   it, and must therefore clear the mode before replacing a file it wrote itself.
//! * **A write of an object that already exists does nothing.** That is not an optimisation: it is
//!   the reason two processes can write the same object concurrently and both succeed, and it is
//!   why the mtime of an unmodified object does not change when a no-op command runs.
//! * **Stray files are normal.** An interrupted write leaves `tmp_obj_*` names, and a corrupted
//!   repository can hold files whose names are not hex at all. Enumeration must skip them rather
//!   than fail, because a single stray file in `objects/` would otherwise make `fsck` impossible
//!   to run — which is exactly when it is needed.
//!
//! # Why not one file per object forever
//!
//! A loose object is a separate zlib stream with its own header, its own inode and its own
//! filesystem metadata, and nothing is shared between two similar objects. That is why `git gc`
//! exists and why [`crate::odb::Odb`] reads packs first for anything it did not just write.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{Object, ObjectKind};

/// Phase 10 — the loose half of the object database: `objects/xx/yyy…` files.
///
/// # Caveats
///
/// * **Cheap to construct and does no I/O.** `open` records the directory and the format; nothing
///   is scanned or validated, so a store over a non-existent directory is constructible and simply
///   finds no objects. Validating at construction would make `Odb::open` fail for a repository that
///   Git considers merely empty.
/// * **Paths are derived, never cached.** Every accessor computes the two-hex directory from the id
///   again. That is a handful of arithmetic per call, and it is what keeps the store a plain value
///   with no interior mutability — the only expensive thing here is the filesystem syscall, which
///   caching a path would not avoid.
/// * **`format` is supplied, not detected.** A loose object's path is two hex characters wide
///   whichever format wrote it, so nothing about the file says whether the id is 20 or 32 bytes.
///   A store opened with the wrong format computes paths that do not exist, which reads as "no
///   objects in this repository" rather than as an error.
/// * **Single-threaded assumptions.** Writes are atomic against other processes, but two threads
///   sharing one store and writing the same object each do their own temp file and rename; the
///   rename is the synchronisation, and it is correct, but a caller expecting exactly one write to
///   happen is not.
///
/// # Panics
///
/// None. A path that cannot be built or a file that cannot be read is an [`crate::error::Error`].
#[derive(Clone, Debug)]
pub struct LooseObjectStore {
    /// The `objects` directory. Loose objects sit in two-hex subdirectories of it.
    objects_dir: PathBuf,
    /// The repository's id width, supplied by the caller from `extensions.objectFormat`.
    format: ObjectFormat,
}

impl LooseObjectStore {
    /// Phase 10 — open a loose object store over an `objects` directory.
    ///
    /// # Caveats
    ///
    /// The path is the objects directory — the one containing the two-hex subdirectories — not the
    /// `.git` directory above it. Nothing is checked to exist: the store is a path and a width, and
    /// a repository with no loose objects is the state every clone is in after `gc`.
    pub fn open(objects_dir: &Path, format: ObjectFormat) -> Self {
        todo!()
    }

    /// Phase 10 — the objects directory this store reads and writes.
    pub fn objects_dir(&self) -> &Path {
        todo!()
    }

    /// Phase 10 — the repository's id width, as it was supplied.
    pub fn format(&self) -> ObjectFormat {
        todo!()
    }

    /// Phase 10 — the path an object with this id has, or would have.
    ///
    /// # Caveats
    ///
    /// * **Derived, not looked up.** The path is returned whether or not the file exists, and no
    ///   syscall is made — which is what lets [`LooseObjectStore::contains`] be a stat rather than
    ///   an open, and a `cat-file -s` on a missing object be cheap.
    /// * **A `PathBuf` here, unlike paths elsewhere in the crate.** The two-hex directory and the
    ///   hex filename are derived from the id and are therefore always ASCII, so there is no
    ///   non-UTF-8 byte string to preserve — the argument that makes tree entry names `BString`
    ///   does not apply. (The filter in [`LooseObjectStore::oids`] is what has to cope with *other
    ///   people's* non-hex filenames, and it does so by skipping them.)
    /// * The parent directory is not created. Writing is what creates it, because a read that
    ///   created directories would make a read-only repository writable.
    pub fn path_for(&self, oid: &ObjectId) -> PathBuf {
        todo!()
    }

    /// Phase 10 — whether the object file exists.
    ///
    /// # Caveats
    ///
    /// * A `stat`, not a parse: a file that exists but is corrupt, truncated or mode-unreadable
    ///   answers `true`. Existence and readability are different questions and only the second one
    ///   needs the bytes.
    /// * An id of the wrong format for this store produces a path under a directory that cannot
    ///   exist, so the answer is `false` — indistinguishable from "not present", which is the best
    ///   that can be done without checking the format at every call.
    pub fn contains(&self, oid: &ObjectId) -> bool {
        todo!()
    }

    /// Phase 10 — read and verify one loose object.
    ///
    /// # Caveats
    ///
    /// * **The full verification sequence is here**, in this order: read the file, inflate it,
    ///   parse the `<type> <length>\0` header, check the declared length equals the remaining byte
    ///   count, rehash the decompressed header and body together, compare against `oid`. Skipping
    ///   or reordering any of it produces a function that either accepts corruption or rejects
    ///   valid objects.
    /// * **The id is verified against the id asked for**, not against a re-derived id stored
    ///   anywhere — there is nowhere to store it. The comparison is what makes `oid` trustworthy;
    ///   without it, the returned [`Object`] is only as good as the filename.
    /// * **The whole file is read into memory**, so a huge blob costs its decompressed size. That
    ///   is unavoidable for a caller that wants an [`Object`], and [`LooseObjectStore::read_header`]
    ///   exists for callers that only want the type and size.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if the file does not exist,
    /// [`crate::error::Error::IoAtPath`] if it cannot be read,
    /// [`crate::error::Error::CorruptObject`] if the zlib stream is truncated or malformed, the
    /// header is missing or unparseable, or the declared length disagrees with the body,
    /// [`crate::error::Error::HashMismatch`] if the content hashes to a different id, and
    /// [`crate::error::Error::MalformedObject`] if the body does not follow its kind's grammar —
    /// though note that this last one requires parsing, which a read does not do.
    pub fn read(&self, oid: &ObjectId) -> Result<Object> {
        todo!()
    }

    /// Phase 10 — read only an object's kind and size, inflating as little as possible.
    ///
    /// Backs `cat-file -t` and `cat-file -s`. The header is at the front of the zlib stream, so a
    /// streaming inflate can stop as soon as the NUL is seen — worth doing for a repository holding
    /// a large blob, and the reason this is not just `read` followed by a length.
    ///
    /// # Caveats
    ///
    /// * **No hash check is possible**, because the body is never read. This is the one read path
    ///   that can report on a corrupt object, which is intentional: it is how `fsck` and a
    ///   suspicious user find out that an object is corrupt. It must therefore not pretend to the
    ///   caller that the object is good.
    /// * **The size is the body's, not the file's**, and not the header's length field alone —
    ///   though for a well-formed object those agree, and a disagreement is
    ///   [`crate::error::Error::CorruptObject`] rather than a lie reported to the user.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`], [`crate::error::Error::IoAtPath`] and
    /// [`crate::error::Error::CorruptObject`] as in [`LooseObjectStore::read`], but never
    /// [`crate::error::Error::HashMismatch`].
    pub fn read_header(&self, oid: &ObjectId) -> Result<(ObjectKind, u64)> {
        todo!()
    }

    /// Phase 10 — write an owned [`Object`], returning its id.
    ///
    /// # Caveats
    ///
    /// * **The id is computed by hashing the header and the body together** — exactly as
    ///   [`crate::object::hash_object`] does, and that function is what this should call rather than
    ///   re-deriving. Computing it from the body alone yields an id that no other Git agrees with.
    /// * **An existing file is left alone.** The object is not rewritten, so its mtime, mode and
    ///   inode are unchanged; this is what makes a concurrent duplicate write harmless, and it is
    ///   also the only thing standing between a hash collision and the destruction of the original
    ///   object — which is precisely what collision detection is for.
    /// * **Temp file, then rename**, with the temp file in the same directory as the target so the
    ///   rename is atomic. The temp name must not be derivable from the object id in a way that two
    ///   processes could collide on.
    /// * **Written read-only afterwards.** The mode is set before the rename, so the object is
    ///   never visible as writable; a caller that then wants to replace it must clear the mode
    ///   first, and only a `gc` that is deleting it should need to.
    /// * **The `xx/` directory is created if absent**, which is the one place a read-only repository
    ///   can fail on write rather than on read.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the directory cannot be created, the temp file cannot be
    /// written or the rename fails, and hasher collision detection from computing the id.
    pub fn write(&self, object: &Object) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 10 — write from a kind and a raw body.
    ///
    /// The path for a caller that has the bytes and not an [`Object`] — a work tree file, a
    /// `hash-object -w` argument, a base recovered while completing a thin pack.
    ///
    /// # Caveats
    ///
    /// **`kind` is trusted**, so a caller that guesses wrong files the content under an id derived
    /// from the wrong header. The object is still internally consistent — it rehashes to the id it
    /// was filed under — but it is not the object the caller meant, and every reference to that
    /// object resolves to nothing. This is the one place in the store where a mistake is silent.
    ///
    /// # Errors
    ///
    /// As [`LooseObjectStore::write`].
    pub fn write_raw(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 10 — delete a loose object.
    ///
    /// Only `rivet gc` needs this, and only after the object has been written into a pack that is
    /// durable on disk.
    ///
    /// # Caveats
    ///
    /// * **Not idempotent by accident: a missing file is an error here.** `gc` deleting an object
    ///   that a concurrent process already packed away is a race it must handle, so the honest
    ///   answer is [`crate::error::Error::ObjectNotFound`] and the caller decides whether to ignore
    ///   it. Making this a silent success would hide a genuinely wrong id.
    /// * **The file is read-only**, so the mode must be cleared before removal.
    /// * **Deleting an object that is still referenced destroys the repository.** Nothing here
    ///   checks reachability — that is the caller's entire job, and doing it wrong is the one way
    ///   `gc` loses data.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if it is not there, and
    /// [`crate::error::Error::IoAtPath`] if it cannot be removed.
    pub fn remove(&self, oid: &ObjectId) -> Result<()> {
        todo!()
    }

    /// Phase 10 — every loose object id present.
    ///
    /// Backs `fsck` and the loose half of a `gc`, both of which need the whole set before they can
    /// decide anything about it.
    ///
    /// # Caveats
    ///
    /// * **Skips anything that is not a well-formed loose object path**: names that are not hex,
    ///   the wrong length for the format, `tmp_obj_*` leftovers from an interrupted write, and
    ///   files directly in `objects/` rather than in a subdirectory. A single stray file must not
    ///   make this fail — that is exactly the repository state in which someone runs `fsck`.
    /// * **Does not read any object**, so the ids returned are filenames. A file whose name is a
    ///   valid id but whose content hashes to something else is listed here and rejected by
    ///   [`LooseObjectStore::read`]; that is the intended division, and it is why `fsck` reads
    ///   everything it lists.
    /// * **Not sorted** — the order is the filesystem's, which differs between machines and
    ///   filesystems. Callers needing a canonical order sort themselves.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if `objects/` or a subdirectory cannot be listed, and
    /// [`crate::error::Error::InvalidHash`] never — a malformed name is skipped, not reported.
    pub fn oids(&self) -> Result<Vec<ObjectId>> {
        todo!()
    }
}

/// Phase 10 — parse the `<type> <length>\0` header of a decompressed loose object.
///
/// Shared between [`LooseObjectStore::read`] and [`LooseObjectStore::read_header`], which both
/// need the kind, the declared length and the offset the body starts at.
///
/// # Caveats
///
/// * **Returns the header's byte length as well**, so the body is a slice from that offset rather
///   than the result of a search for the NUL — the body may contain NULs, and searching from the
///   wrong place finds one of those and silently truncates the object.
/// * **The declared length is *not* checked against the body here.** This function only sees the
///   front of the stream; the comparison is the caller's, and it is what catches a truncated
///   object. A parser that trusted the declared length would let a truncated object through with a
///   plausible type and size.
/// * The kind keyword is matched byte-exactly and in lowercase; `"BLOB"` or `"blob "` is
///   corruption, not an alternative spelling.
/// * The length is decimal ASCII with no sign, no leading `+`, and no leading zeros beyond a bare
///   `0` — Git writes the shortest form, and accepting variants would mean accepting objects that
///   hash differently from what Rivet would write.
///
/// # Errors
///
/// [`crate::error::Error::CorruptObject`] if there is no NUL byte, the keyword is not one of the
/// four kinds, or the length is empty, non-numeric, or overflows a `u64`.
pub(crate) fn parse_loose_header(data: &[u8]) -> Result<(ObjectKind, u64, usize)> {
    todo!()
}
