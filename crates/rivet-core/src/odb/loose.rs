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

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::ZlibDecoder;

use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{hash_object, Object, ObjectKind};
use bstr::BString;

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
        Self { objects_dir: objects_dir.to_path_buf(), format }
    }

    /// Phase 10 — the objects directory this store reads and writes.
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Phase 10 — the repository's id width, as it was supplied.
    pub fn format(&self) -> ObjectFormat {
        self.format
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
        let hex = oid.to_hex();
        let hex_str = std::str::from_utf8(&hex).unwrap();
        self.objects_dir.join(&hex_str[..2]).join(&hex_str[2..])
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
        fs::metadata(self.path_for(oid)).is_ok()
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
        let path = self.path_for(oid);
        let path_str = path.to_string_lossy();
        let data = match fs::read(&path) {
            Ok(data) => data,
            // A loose object that is not on disk is a plain miss, not an I/O failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::ObjectNotFound { oid: *oid })
            }
            Err(e) => {
                return Err(Error::IoAtPath {
                    path: BString::from(path_str.as_ref()),
                    source: e,
                })
            }
        };

        let mut inflator = ZlibDecoder::new(data.as_slice());
        let mut body = Vec::with_capacity(data.len());
        inflator
            .read_to_end(&mut body)
            .map_err(|e| Error::CorruptObject { oid: *oid, reason: format!("bad zlib stream: {e}") })?;

        let (kind, declared, header_len) =
            parse_loose_header(&body).map_err(|e| match e {
                Error::CorruptObject { reason, .. } => Error::CorruptObject { oid: *oid, reason },
                other => other,
            })?;

        let actual = u64::try_from(body.len()).unwrap() - u64::try_from(header_len).unwrap();
        if actual != declared {
            return Err(Error::CorruptObject {
                oid: *oid,
                reason: format!("declared length {declared} does not match actual body length {actual}"),
            });
        }

        let payload = &body[header_len..];
        let (actual_id, _) = hash_object(self.format, kind, payload).map_err(|e| match e {
            Error::CorruptObject { reason, .. } => Error::CorruptObject { oid: *oid, reason },
            other => other,
        })?;

        if actual_id != *oid {
            return Err(Error::CorruptObject {
                oid: *oid,
                reason: format!("content hashes to {actual_id}, expected {oid}"),
            });
        }

        let obj = match kind {
            ObjectKind::Blob => Object::Blob(payload.to_vec()),
            ObjectKind::Tree => Object::Tree(payload.to_vec()),
            ObjectKind::Commit => Object::Commit(payload.to_vec()),
            ObjectKind::Tag => Object::Tag(payload.to_vec()),
        };
        Ok(obj)
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
        let path = self.path_for(oid);
        let path_str = path.to_string_lossy();
        let data = match fs::read(&path) {
            Ok(data) => data,
            // A loose object that is not on disk is a plain miss, not an I/O failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::ObjectNotFound { oid: *oid })
            }
            Err(e) => {
                return Err(Error::IoAtPath {
                    path: BString::from(path_str.as_ref()),
                    source: e,
                })
            }
        };

        let mut inflator = ZlibDecoder::new(data.as_slice());
        let mut buf = [0u8; 8192];
        let mut header = Vec::new();
        let mut nul_seen = false;
        let mut remaining = 0u64;
        loop {
            let n = inflator
                .read(&mut buf)
                .map_err(|e| Error::CorruptObject {
                    oid: *oid,
                    reason: format!("bad zlib stream: {e}"),
                })?;
            if n == 0 {
                break;
            }
            if !nul_seen {
                let chunk = &buf[..n];
                match chunk.iter().position(|&b| b == 0) {
                    Some(i) => {
                        header.extend_from_slice(&chunk[..=i]);
                        remaining = n as u64 - (i + 1) as u64;
                        nul_seen = true;
                    }
                    None => {
                        header.extend_from_slice(chunk);
                    }
                }
            } else {
                remaining += n as u64;
            }
        }

        let (kind, declared, _header_len) =
            parse_loose_header(&header).map_err(|e| match e {
                Error::CorruptObject { reason, .. } => Error::CorruptObject { oid: *oid, reason },
                other => other,
            })?;

        let actual = remaining;
        if actual != declared {
            return Err(Error::CorruptObject {
                oid: *oid,
                reason: format!("declared length {declared} does not match actual body length {actual}"),
            });
        }

        Ok((kind, actual))
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
        self.write_raw(object.kind(), object.data())
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
        let (oid, uncompressed) = hash_object(self.format, kind, data)?;

        let path = self.path_for(&oid);
        if path.exists() {
            return Ok(oid);
        }

        let dir = path.parent().unwrap();
        let dir_str = dir.to_string_lossy();
        fs::create_dir_all(dir).map_err(|e| Error::IoAtPath {
            path: BString::from(dir_str.as_ref()),
            source: e,
        })?;

        // Temp name is not derived from the object id, so two concurrent writers
        // of different objects cannot collide on it.
        let tmp = dir.join(format!(
            "tmp_obj_{}_{}",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().unwrap_or_default().as_nanos()
        ));
        let tmp_str = tmp.to_string_lossy();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| Error::IoAtPath {
                path: BString::from(tmp_str.as_ref()),
                source: e,
            })?;
        // Loose objects live on disk zlib-compressed. The id is over the
        // *uncompressed* `<kind> <len>\0<body>` bytes, so compress only here,
        // at the last step before the bytes hit the filesystem.
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&uncompressed).map_err(|e| Error::IoAtPath {
            path: BString::from(tmp_str.as_ref()),
            source: e,
        })?;
        let compressed = enc.finish().map_err(|e| Error::IoAtPath {
            path: BString::from(tmp_str.as_ref()),
            source: e,
        })?;
        file.write_all(&compressed).map_err(|e| Error::IoAtPath {
            path: BString::from(tmp_str.as_ref()),
            source: e,
        })?;
        // Set the file read-only (0444), Git's convention for loose objects.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o444);
            file.set_permissions(perms).map_err(|e| Error::IoAtPath {
                path: BString::from(tmp_str.as_ref()),
                source: e,
            })?;
        }
        file.flush().map_err(|e| Error::IoAtPath {
            path: BString::from(tmp_str.as_ref()),
            source: e,
        })?;
        drop(file);

        let path_str = path.to_string_lossy();
        fs::rename(&tmp, &path).map_err(|e| {
            // Best-effort cleanup of the temp file on rename failure.
            let _ = fs::remove_file(&tmp);
            Error::IoAtPath {
                path: BString::from(path_str.as_ref()),
                source: e,
            }
        })?;

        Ok(oid)
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
        let path = self.path_for(oid);
        if !path.exists() {
            return Err(Error::ObjectNotFound { oid: *oid });
        }

        let path_str = path.to_string_lossy();
        // Files are read-only (0444), so the mode must be cleared first.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o644);
            fs::set_permissions(&path, perms).map_err(|e| Error::IoAtPath {
                path: BString::from(path_str.as_ref()),
                source: e,
            })?;
        }
        fs::remove_file(&path).map_err(|e| Error::IoAtPath {
            path: BString::from(path_str.as_ref()),
            source: e,
        })
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
        let mut ids = Vec::new();
        let objects_str = self.objects_dir.to_string_lossy();
        let entries = match fs::read_dir(&self.objects_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            Err(e) => return Err(Error::IoAtPath {
                path: BString::from(objects_str.as_ref()),
                source: e,
            }),
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => return Err(Error::IoAtPath {
                    path: BString::from(objects_str.as_ref()),
                    source: e,
                }),
            };
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = match path.file_name() {
                Some(name) => name,
                None => continue,
            };
            let name = match name.to_str() {
                Some(name) if name.len() == 2 => name,
                _ => continue,
            };
            if !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let dir_str = path.to_string_lossy();
            for file in fs::read_dir(&path).map_err(|e| Error::IoAtPath {
                path: BString::from(dir_str.as_ref()),
                source: e,
            })? {
                let file = match file {
                    Ok(file) => file,
                    Err(e) => return Err(Error::IoAtPath {
                        path: BString::from(dir_str.as_ref()),
                        source: e,
                    }),
                };
                let path = file.path();
                if !path.is_file() {
                    continue;
                }
                let fname = match path.file_name() {
                    Some(fname) => fname,
                    None => continue,
                };
                let fname = match fname.to_str() {
                    Some(fname) => fname,
                    None => continue,
                };
                if fname.len() != self.format.hex_len() - 2 {
                    continue;
                }
                if !fname.bytes().all(|b| b.is_ascii_hexdigit()) {
                    continue;
                }
                let full = name.to_string() + fname;
                if let Ok(oid) = ObjectId::from_hex(self.format, full.as_bytes().into()) {
                    ids.push(oid);
                }
            }
        }

        Ok(ids)
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
    let nul = data
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject {
            oid: ObjectId::zero(ObjectFormat::Sha1),
            reason: "no NUL byte terminating the header".to_string(),
        })?;

    let header = &data[..nul];

    // Split "kind len" — the space is the only separator.
    let space = header
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| Error::CorruptObject {
            oid: ObjectId::zero(ObjectFormat::Sha1),
            reason: "no space in header".to_string(),
        })?;

    let kind_bytes = &header[..space];
    let kind = ObjectKind::from_bytes(kind_bytes).map_err(|e| Error::CorruptObject {
        oid: ObjectId::zero(ObjectFormat::Sha1),
        reason: format!("invalid kind {kind_bytes:?}: {e}"),
    })?;

    let len_bytes = &header[space + 1..];
    if len_bytes.is_empty() {
        return Err(Error::CorruptObject {
            oid: ObjectId::zero(ObjectFormat::Sha1),
            reason: "empty length in header".to_string(),
        });
    }
    if !len_bytes.iter().all(|&b| b.is_ascii_digit()) {
        return Err(Error::CorruptObject {
            oid: ObjectId::zero(ObjectFormat::Sha1),
            reason: "non-numeric length in header".to_string(),
        });
    }
    // No leading zeros beyond a bare "0": the shortest form is what Git writes.
    if len_bytes.len() > 1 && len_bytes[0] == b'0' {
        return Err(Error::CorruptObject {
            oid: ObjectId::zero(ObjectFormat::Sha1),
            reason: "leading zero in header length".to_string(),
        });
    }

    let declared: u64 = len_bytes.iter().try_fold(0u64, |acc, &b| {
        acc.checked_mul(10)
            .and_then(|a| a.checked_add(u64::from(b - b'0')))
    })
    .ok_or_else(|| Error::CorruptObject {
        oid: ObjectId::zero(ObjectFormat::Sha1),
        reason: "length overflows u64".to_string(),
    })?;

    Ok((kind, declared, nul + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use std::os::unix::fs::PermissionsExt;

    fn store(dir: &Path, fmt: ObjectFormat) -> LooseObjectStore {
        LooseObjectStore::open(dir, fmt)
    }

    fn write_and_read_roundtrip(dir: &Path, fmt: ObjectFormat, kind: ObjectKind, data: &[u8]) {
        let s = store(dir, fmt);
        let oid = s.write_raw(kind, data).expect("write failed");
        let obj = s.read(&oid).expect("read failed");
        assert_eq!(obj.kind(), kind);
        assert_eq!(obj.data(), data);
        assert!(s.contains(&oid));
        // path layout: objects/xx/yyyy…
        let hex = oid.to_hex();
        let hex_str = std::str::from_utf8(&hex).expect("oid hex is utf-8");
        let expected = dir.join(&hex_str[..2]).join(&hex_str[2..]);
        assert_eq!(s.path_for(&oid), expected);
    }

    #[test]
    fn roundtrip_all_kinds_sha1() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        let fmt = ObjectFormat::Sha1;
        write_and_read_roundtrip(&dir, fmt, ObjectKind::Blob, b"hello\n");
        write_and_read_roundtrip(&dir, fmt, ObjectKind::Tree, b"\x0100000000blob\0hello\n");
        write_and_read_roundtrip(&dir, fmt, ObjectKind::Commit, b"tree 0000000000000000000000000000000000000000\nauthor A <a@b> 0 +0000\ncommitter A <a@b> 0 +0000\n\nmsg\n");
        write_and_read_roundtrip(&dir, fmt, ObjectKind::Tag, b"object 0000000000000000000000000000000000000000\ntype commit\n");
    }

    #[test]
    fn roundtrip_all_kinds_sha256() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        let fmt = ObjectFormat::Sha256;
        write_and_read_roundtrip(&dir, fmt, ObjectKind::Blob, b"hello\n");
        write_and_read_roundtrip(&dir, fmt, ObjectKind::Tree, b"\x0100000000blob\0hello\n");
        write_and_read_roundtrip(&dir, fmt, ObjectKind::Commit, b"tree 0000000000000000000000000000000000000000000000000000000000000000\nauthor A <a@b> 0 +0000\ncommitter A <a@b> 0 +0000\n\nmsg\n");
        write_and_read_roundtrip(&dir, fmt, ObjectKind::Tag, b"object 0000000000000000000000000000000000000000000000000000000000000000\ntype commit\n");
    }

    #[test]
    fn known_git_id_sha1() {
        // blob "hello\n" must hash to Git's canonical id
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        let s = store(&dir, ObjectFormat::Sha1);
        let oid = s.write_raw(ObjectKind::Blob, b"hello\n").unwrap();
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    #[test]
    fn read_truncated_file_is_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        let s = store(&dir, ObjectFormat::Sha1);
        let oid = s.write_raw(ObjectKind::Blob, b"hello\n").unwrap();
        let path = s.path_for(&oid);

        // Truncate the file to half its size.
        let data = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, &data[..data.len() / 2]).unwrap();

        let err = s.read(&oid).unwrap_err();
        match err {
            Error::CorruptObject { oid: eoid, .. } => assert_eq!(eoid, oid),
            _ => panic!("expected CorruptObject, got {err:?}"),
        }
    }

    #[test]
    fn corrupt_object_error_names_requested_id() {
        // Write a valid object, then corrupt its content so it no longer hashes to its id.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        let s = store(&dir, ObjectFormat::Sha1);
        let oid = s.write_raw(ObjectKind::Blob, b"hello\n").unwrap();
        let path = s.path_for(&oid);

        // Replace the file content with a zlib stream whose decompressed body
        // has the same header but a different payload.
        let mut compressed = Vec::new();
        let mut enc = ZlibEncoder::new(&mut compressed, flate2::Compression::default());
        enc.write_all(b"blob 5\0XXXXX").unwrap();
        enc.finish().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, &compressed).unwrap();

        let err = s.read(&oid).unwrap_err();
        match err {
            Error::CorruptObject { oid: eoid, .. } => assert_eq!(eoid, oid),
            _ => panic!("expected CorruptObject, got {err:?}"),
        }
    }

    #[test]
    fn write_same_object_twice_no_rewrite() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        let s = store(&dir, ObjectFormat::Sha1);
        let oid = s.write_raw(ObjectKind::Blob, b"hello\n").unwrap();
        let path = s.path_for(&oid);
        let mtime1 = fs::metadata(&path).unwrap().modified().unwrap();
        // small sleep so mtime would differ if rewritten
        std::thread::sleep(std::time::Duration::from_millis(50));
        let oid2 = s.write_raw(ObjectKind::Blob, b"hello\n").unwrap();
        assert_eq!(oid, oid2);
        let mtime2 = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2);
    }

    #[test]
    fn remove_then_read_is_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        let s = store(&dir, ObjectFormat::Sha1);
        let oid = s.write_raw(ObjectKind::Blob, b"hello\n").unwrap();
        s.remove(&oid).unwrap();
        assert!(!s.contains(&oid));
        let err = s.read(&oid).unwrap_err();
        match err {
            Error::ObjectNotFound { oid: eoid, .. } => assert_eq!(eoid, oid),
            _ => panic!("expected ObjectNotFound, got {err:?}"),
        }
    }

    #[test]
    fn oids_lists_written_ids_and_skips_junk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        let s = store(&dir, ObjectFormat::Sha1);
        let oid1 = s.write_raw(ObjectKind::Blob, b"hello\n").unwrap();
        let oid2 = s.write_raw(ObjectKind::Blob, b"world\n").unwrap();

        // Drop a stray junk file at the top level and inside a two-hex dir.
        fs::write(dir.join("junk"), b"not hex").unwrap();
        let hex1 = oid1.to_hex();
        let hex1_str = std::str::from_utf8(&hex1).expect("oid hex is utf-8");
        let sub = dir.join(&hex1_str[..2]);
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("junk2"), b"also not valid").unwrap();
        // A subdirectory inside a two-hex dir (not a file) — skipped.
        fs::create_dir_all(sub.join("subdir")).unwrap();

        let ids = s.oids().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&oid1));
        assert!(ids.contains(&oid2));
    }

    #[test]
    fn oids_empty_when_dir_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("objects");
        // dir does not exist
        let s = store(&dir, ObjectFormat::Sha1);
        let ids = s.oids().unwrap();
        assert!(ids.is_empty());
    }
}
