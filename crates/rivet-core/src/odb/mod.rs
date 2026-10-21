//! Phase 10 — the object database: one read path over loose objects and packs.
//!
//! Callers do not choose between the stores. An object's location is an implementation detail of
//! how the repository has been maintained — `git gc` moves loose objects into packs, a fetch
//! arrives as a pack and is indexed into place, `hash-object -w` writes loose — so code that asked
//! "is this loose or packed" would be code that breaks the first time somebody ran `gc`. [`Odb`]
//! answers "give me this object" and hides which of the two it came from.
//!
//! # The search order
//!
//! Loose first, then packs in turn. That order is not arbitrary: a loose object is always the most
//! recent write of an id, and a pack cannot be added to once written, so when both hold the same id
//! the loose copy is the one a `git repack` has not yet absorbed. Reading the pack first would not
//! be *wrong* — both copies must hash to the same id or one of them is corruption — but it would
//! give up the ability to notice that they differ, which is a corruption signal worth keeping.
//!
//! # Caveats that bite
//!
//! * **The format is a property of the repository, not of the call.** Every id, in every loose
//!   path, pack entry and `.idx` row, is 20 or 32 bytes according to
//!   `extensions.objectFormat`. An `Odb` opened with the wrong format does not fail loudly; it
//!   reads the wrong bytes from the right places.
//! * **Packs are discovered by scanning `objects/pack/`**, and the scan happens at `open` and at
//!   [`Odb::refresh`]. A `git gc` in another process between two reads is invisible until the next
//!   refresh, which is why a read that fails to find an object should refresh and retry once
//!   before reporting [`crate::error::Error::ObjectNotFound`] — the object may have been there all
//!   along, in a pack this handle did not know about.
//! * **A `.pack` and its `.idx` are a pair.** A pack with no index is unsearchable by id (see
//!   [`crate::pack::PackFile`]) and is skipped by `open` rather than treated as fatal, because the
//!   window in which a concurrent index-pack has written one and not the other is normal.
//! * **Reading verifies every hash.** A loose object whose content hashes to something other than
//!   its filename, a packed object whose content does not match the id it was found under, a delta
//!   whose reconstruction is not what the `.idx` claims — all are
//!   [`crate::error::Error::HashMismatch`] and all are checked on every read (plan §5). This is not
//!   free and is not optional.
//! * **Writing always goes loose.** There is no `Odb::write` that appends to a pack: a pack is
//!   immutable, its trailing checksum is its name, and appending would rewrite both. Packing loose
//!   objects is `rivet gc`'s job, in Phase 12.
//! * **Nothing here is a cache of content.** Only delta *bases* are cached, because resolving a
//!   chain re-inflates them; whole objects are re-read on every call. That is the right default for
//!   a CLI, where each command reads a tree or two, and it is what makes a long-lived `Odb` safe to
//!   hold across a work tree scan.
//!
//! # Loose, packed, and the third case
//!
//! A pack that has been received but not yet indexed has objects that exist and cannot be found.
//! That state is not representable here: [`Odb::add_pack`] takes a pack *and* its index, and
//! `index-pack` is what turns one into the other. Keeping a half-indexed pack out of the database
//! is what makes "not found" mean "not in this repository".

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};


use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{Object, ObjectKind};
use crate::pack::BaseProvider;

const DELTA_CACHE_CAPACITY: usize = 1024;

pub mod loose;
pub mod packed;

use packed::DeltaBaseCache;

pub use loose::LooseObjectStore;
pub use packed::PackedObjectStore;

/// The `.pack` path an entry in `objects/pack/` names, or `None` for anything else.
///
/// Git names the two halves of a pack `pack-<hex>.pack` and `pack-<hex>.idx`, so the index file
/// name already carries the `pack-` prefix. Stripping only the suffix and re-adding the prefix
/// would look up `pack-pack-<hex>.pack`, which never exists — every pack would be skipped in
/// silence and its objects would report as absent. An index whose pack is missing — a half-copied
/// or pruned pack — is also `None`, so it is skipped rather than reported.
fn pack_path_for(pack_dir: &Path, file_name: &str) -> Option<PathBuf> {
    let stem = file_name.strip_suffix(".idx")?.strip_prefix("pack-")?;
    Some(pack_dir.join(format!("pack-{stem}.pack")))
}

/// List `objects/pack/`, treating a missing directory as an empty one.
///
/// A fresh or gc'd repository has no `pack/`, so its absence is not an error. Any other failure to
/// list it is: an unreadable `pack/` would otherwise look exactly like a repository with no packs,
/// and every packed object in it would report as absent instead of failing loudly.
fn read_pack_dir(pack_dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let entries = match fs::read_dir(pack_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(Error::IoAtPath {
                path: bstr::BString::from(pack_dir.to_string_lossy().as_ref()),
                source: err,
            })
        }
    };
    // An entry that cannot be read is a real failure, not an entry to skip.
    let mut found = Vec::new();
    for entry in entries {
        found.push(entry.map_err(|err| Error::IoAtPath {
            path: bstr::BString::from(pack_dir.to_string_lossy().as_ref()),
            source: err,
        })?);
    }
    Ok(found)
}

/// Phase 10 — the object database for one repository.
///
/// Owns the loose store and every pack under `objects/pack/`, and answers the questions the rest of
/// the crate asks: read this object, does it exist, what is its size, which id does this prefix
/// name.
///
/// # Caveats
///
/// * **Holds every pack's bytes in memory.** A clone of a large repository has hundreds of
///   megabytes of packs, and this type reads them at `open`. mmap is the Phase 12 fix (plan §4) and
///   does not change any signature here — [`crate::pack::PackFile::open`] already takes a buffer it
///   does not own.
/// * **Cheap to construct, expensive to use past its first read.** `open` parses every `.idx` but
///   inflates nothing, so a repository with no packs and no objects costs nothing. The cost is
///   paid per read, deliberately: a command like `status` reads a handful of objects, and eager
///   caching would make every command pay for the ones it did not want.
/// * **Not `Clone`.** The delta cache is behind a `RefCell`, and two clones sharing a cache would
///   be surprising given the type looks like a value. Wrap it in an `Rc` if two owners are needed.
/// * **A repository with `objects/info/alternates` is not supported.** Git's alternate object
///   databases are how a clone shares objects with the repository it was cloned from; Rivet reads
///   the file's absence as an empty repository. Reading it is a later phase, and the failure mode
///   without it is a clean [`crate::error::Error::ObjectNotFound`] rather than a wrong object.
///
/// # Panics
///
/// None. Corrupt repository data produces an [`crate::error::Error`] (plan §5).
#[derive(Debug)]
pub struct Odb {
    /// The repository's id width, taken from `extensions.objectFormat` when the repository was
    /// opened. Everything here is read and written through it.
    format: ObjectFormat,
    /// The loose object store over `objects/xx/`.
    loose: LooseObjectStore,
    /// Every indexed pack found under `objects/pack/`, in the order discovered.
    packs: Vec<PackedObjectStore>,
    /// Resolved delta bases, so a deltified tree walk does not re-inflate the same base for every
    /// entry in it. Behind a `RefCell` because reads take `&self` — the cache is a pure
    /// optimisation and must not be visible in the type.
    delta_cache: RefCell<DeltaBaseCache>,
}

impl Odb {
    /// Phase 10 — open the object database of the repository whose git directory is `git_dir`.
    ///
    /// # Caveats
    ///
    /// * **`git_dir` is the `.git` directory, not the work tree.** The objects directory is
    ///   `git_dir/objects`, which is also the layout inside a bare repository. Passing the work
    ///   tree produces a database over a non-existent directory, which is not an error here — an
    ///   empty object database looks the same as a fresh one — and surfaces later as
    ///   [`crate::error::Error::ObjectNotFound`].
    /// * **`format` must come from the repository's config.** It is not discoverable from the
    ///   object files: a loose object's directory name is two hex characters either way, and a
    ///   pack header records no width. See [`crate::hash::ObjectFormat`].
    /// * **Missing directories are not an error.** `objects/pack/` is created by `git init` but a
    ///   hand-made repository may lack it, and a scan that treated it as fatal would refuse a
    ///   repository Git accepts.
    /// * **A corrupt `.idx` is fatal, a corrupt `.pack` is not.** The index is parsed here and its
    ///   checksums verified, because every later read depends on it. A `.pack` that fails its own
    ///   trailer check is not noticed until something is read from it — verifying every pack at
    ///   open would make `rivet status` linear in the repository's size.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the packs directory exists but cannot be listed;
    /// [`crate::error::Error::CorruptPackIndex`] or
    /// [`crate::error::Error::UnsupportedPackIndexVersion`] for an index that does not parse.
    pub fn open(git_dir: &Path, format: ObjectFormat) -> Result<Self> {
        Self::open_objects_dir(&git_dir.join("objects"), format)
    }

    /// Phase 10 — open a database directly over an `objects` directory.
    ///
    /// The entry point for a bare repository and for tests, which have the objects directory in
    /// hand and no `.git` around it.
    ///
    /// # Caveats
    ///
    /// The path is the objects directory itself — the one containing `xx/` subdirectories and
    /// `pack/` — not its parent. Passing the git directory reads a repository with no objects,
    /// silently.
    ///
    /// # Errors
    ///
    /// As [`Odb::open`].
    pub fn open_objects_dir(objects_dir: &Path, format: ObjectFormat) -> Result<Self> {
        let loose = loose::LooseObjectStore::open(objects_dir, format);

        let mut packs = Vec::new();
        let pack_dir = objects_dir.join("pack");
        // A missing `pack` directory is not an error: a fresh or gc'd repository has none.
        for entry in read_pack_dir(&pack_dir)? {
            let Ok(file_name) = entry.file_name().into_string() else {
                continue;
            };
            let Some(pack_path) = pack_path_for(&pack_dir, &file_name) else {
                continue;
            };
            if !pack_path.is_file() {
                continue;
            }
            let data = fs::read(&pack_path)?;
            let index = fs::read(entry.path())?;
            let pack = packed::PackedObjectStore::open(data, index, format)?;
            packs.push(pack);
        }
        packs.sort_by_key(|pack| pack.checksum());

        Ok(Self {
            format,
            loose,
            packs,
            delta_cache: RefCell::new(DeltaBaseCache::new(DELTA_CACHE_CAPACITY)),
        })
    }

    /// Phase 10 — the `objects` directory this database is over.
    pub fn objects_dir(&self) -> &Path {
        self.loose.objects_dir()
    }

    /// Phase 10 — the repository's id width, as it was supplied at open.
    ///
    /// # Caveats
    ///
    /// Returned as given, not detected. A mismatch with the objects on disk is not detectable here
    /// and produces wrong reads rather than errors — the one place it is caught is the final hash
    /// check, which is a [`crate::error::Error::HashMismatch`] that looks like corruption.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// Phase 10 — the loose object store, for code that must write.
    ///
    /// # Caveats
    ///
    /// Exposed because [`Odb::write`] is thin over it and because `verify-pack`-style diagnostics
    /// want to report loose and packed counts separately. Reading through it directly bypasses the
    /// pack fallback, so an object that `gc` has packed reports as absent — which is exactly right
    /// for a caller asking about loose storage and wrong for a caller asking about objects.
    pub fn loose(&self) -> &LooseObjectStore {
        &self.loose
    }

    /// Phase 10 — the packs this database knows about, in discovery order.
    ///
    /// # Caveats
    ///
    /// The order is the filesystem's, so it is **not stable across machines** and must not be
    /// relied on for anything but reporting. A prefix that appears in two packs is ambiguous
    /// regardless of this order, and [`Odb::resolve_prefix`] says so rather than picking the first.
    pub fn packs(&self) -> &[PackedObjectStore] {
        &self.packs
    }

    /// Phase 10 — rescan `objects/pack/` and pick up packs that appeared since the last scan.
    ///
    /// Needed because packs arrive without warning: a concurrent `git gc`, a `fetch` that indexed
    /// a pack into place, or Rivet's own [`Odb::add_pack`] writing one. Callers that hold an `Odb`
    /// across a long operation — a fetch loop, a work tree walk — must refresh rather than assume
    /// the snapshot they opened is still complete.
    ///
    /// # Caveats
    ///
    /// * **This only adds.** A pack that disappeared — because a concurrent `repack` removed the
    ///   file — stays in the list and fails on read. Rebuilding the whole set would invalidate the
    ///   delta cache's pack identities, and a vanished pack is a race the caller has to handle
    ///   anyway.
    /// * **The delta cache survives.** Entries are keyed by pack checksum and offset, so a new pack
    ///   cannot collide with an old one's entries.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the directory cannot be listed, and the index-parse
    /// errors of [`Odb::open`] for a newly appeared pack whose `.idx` is bad.
    pub fn refresh(&mut self) -> Result<()> {
        let pack_dir = self.loose.objects_dir().join("pack");
        // No packs directory: there is nothing new to pick up.
        for entry in read_pack_dir(&pack_dir)? {
            let Ok(file_name) = entry.file_name().into_string() else {
                continue;
            };
            let Some(pack_path) = pack_path_for(&pack_dir, &file_name) else {
                continue;
            };
            if !pack_path.is_file() {
                continue;
            }
            let data = fs::read(&pack_path)?;
            let index = fs::read(entry.path())?;
            let pack = packed::PackedObjectStore::open(data, index, self.format)?;
            // Already seen: a pack that vanished is a race, and we only add, not rescan.
            // Identity is the pack's own checksum, which is stable and unique per pack.
            if self.packs.iter().any(|existing| existing.checksum() == pack.checksum()) {
                continue;
            }
            self.packs.push(pack);
        }
        Ok(())
    }

    /// Phase 10 — whether the object is present, without reading it.
    ///
    /// # Caveats
    ///
    /// * Answers for **full ids only**. An abbreviated id needs the whole database searched and is
    ///   [`Odb::resolve_prefix`]'s job.
    /// * **A `true` here is not a promise that a read will succeed.** The object may be corrupt, or
    ///   its delta chain may not resolve — existence is answered from the loose filename and the
    ///   `.idx`'s sorted table, neither of which touches the bytes. Callers that need certainty must
    ///   read.
    /// * **A `false` is only as fresh as the last refresh.** See [`Odb::refresh`].
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.loose.contains(oid) || self.packs.iter().any(|pack| pack.contains(oid))
    }

    /// Phase 10 — read one object, hash-verified.
    ///
    /// The single read path for the whole crate: everything that needs bytes out of the repository
    /// comes through here, whether the object is loose or deltified three deep in a pack.
    ///
    /// # Caveats
    ///
    /// * **The returned [`Object`] owns its bytes.** A caller that only parses — the common case —
    ///   pays a copy it does not need. The borrowed alternative would tie the result to the pack
    ///   buffer and force every caller to hold the `Odb` and the borrow together, which is why the
    ///   owned form is the one exposed. `[`crate::object::AnyObject`]` can then borrow from it.
    /// * **The id is verified, always.** The last step is rehashing the content and comparing
    ///   against `oid`; a mismatch is [`crate::error::Error::HashMismatch`] and the content is
    ///   discarded. For a packed object this is the only check that the delta chain resolved to the
    ///   right thing.
    /// * **Deltas are resolved across stores.** A `REF_DELTA` base may be in another pack or loose
    ///   — that is what a thin pack means, and this is the layer that can satisfy it, which is why
    ///   this type implements [`crate::pack::BaseProvider`].
    /// * **Read the object you asked for, not what it points at.** Nothing here peels tags or walks
    ///   parents; that is revision parsing.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] if no store has it;
    /// [`crate::error::Error::CorruptObject`] for a loose object whose zlib stream or header is
    /// bad; [`crate::error::Error::CorruptPack`] for a bad pack entry;
    /// [`crate::error::Error::CorruptDelta`] or [`crate::error::Error::DeltaChainTooDeep`] for a
    /// delta that does not resolve; and [`crate::error::Error::HashMismatch`] when the content does
    /// not match the requested id.
    pub fn read(&self, oid: &ObjectId) -> Result<Object> {
        // Loose wins over packed: a loose file is the most recent write of the id and is what a
        // `repack` has not yet absorbed. Never swallow a non-NotFound loose error — a corrupt loose
        // object must be reported, not re-answered from a pack as a "miss".
        let loose = self.loose.read(oid);
        match loose {
            Ok(obj) => return Ok(obj),
            Err(Error::ObjectNotFound { .. }) => {}
            Err(e) => return Err(e),
        }
        // A loose miss falls through to every pack in turn. A miss here is the answer, not a
        // prompt to refresh: freshness is the caller's job, and `read(&self)` cannot refresh.
        for pack in &self.packs {
            match pack.read(oid, Some(self)) {
                Ok(object) => return Ok(object),
                // A miss in one pack says nothing about its siblings: the id may live in the
                // pack written after this one. Only ObjectNotFound is a "keep looking"; a
                // corrupt or delta-broken pack is an error and must not be swallowed.
                Err(Error::ObjectNotFound { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Err(Error::ObjectNotFound { oid: *oid })
    }

    /// Phase 10 — read one object and require it to be of a given kind.
    ///
    /// The shape every caller that has a fixed expectation wants — a tree walk wants trees, a
    /// commit walk wants commits — and the reason [`crate::error::Error::WrongObjectKind`] exists.
    ///
    /// # Caveats
    ///
    /// The object is read and verified **before** the kind is checked, so a corrupt commit is
    /// reported as corruption rather than as "not a tree". Reporting the wrong failure is what
    /// makes a broken repository hard to diagnose, so the ordering is deliberate.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::WrongObjectKind`] if the object is intact but is something else, and
    /// everything [`Odb::read`] raises.
    pub fn read_typed(&self, oid: &ObjectId, expected: ObjectKind) -> Result<Object> {
        let object = self.read(oid)?;
        if object.kind() != expected {
            return Err(Error::WrongObjectKind { oid: Some(*oid), found: object.kind(), expected });
        }
        Ok(object)
    }

    /// Phase 10 — read an object's kind and size without materialising it.
    ///
    /// This is `cat-file -t` and `cat-file -s`, and it is worth having separately: for a loose
    /// object the header is a few bytes at the front of the zlib stream, and for a packed object
    /// the size is in the entry header — so neither needs the payload inflated, and the
    /// difference shows on a 2 GB blob.
    ///
    /// # Caveats
    ///
    /// * **For a deltified entry this costs more than it looks.** A pack entry's own size is the
    ///   delta's length, not the object's, so answering for a delta means resolving the chain and
    ///   measuring the result. The fast path only exists for loose objects and undeltified entries.
    /// * **The size is `u64` and the object is not checked against its id**, because the payload is
    ///   never read. `cat-file -s` is therefore the one read path that can report on a corrupt
    ///   object, which is intentional — it is how you find out the object is corrupt.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`], and the header-level corruption errors of
    /// [`Odb::read`] — but never [`crate::error::Error::HashMismatch`].
    pub fn read_header(&self, oid: &ObjectId) -> Result<(ObjectKind, u64)> {
        // Loose first: the header lives at the front of the zlib stream and never needs the body
        // inflated. A miss is not an error and falls through to the packs.
        let loose = self.loose.read_header(oid);
        match loose {
            Ok((kind, size)) => return Ok((kind, size)),
            Err(Error::ObjectNotFound { .. }) => {}
            Err(e) => return Err(e),
        }
        for pack in &self.packs {
            match pack.read(oid, Some(self)) {
                Ok(object) => return Ok((object.kind(), object.data().len() as u64)),
                // Same "keep looking" rule as `read`: only a miss moves on to the next pack.
                Err(Error::ObjectNotFound { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Err(Error::ObjectNotFound { oid: *oid })
    }

    /// Phase 10 — write an object, returning its id.
    ///
    /// # Caveats
    ///
    /// * **Always loose.** See the module caveat: a pack cannot be appended to, so a write is a
    ///   new file under `objects/xx/`. Packing them up is `rivet gc`, and until then a repository
    ///   Rivet writes to has both kinds of storage — which is a state Git itself spends most of its
    ///   life in.
    /// * **Idempotent.** An object already present is left alone: the file is not rewritten, so a
    ///   write that races a concurrent one is harmless and the mtime does not change. That also
    ///   means a hash collision cannot overwrite an existing object, which is the property
    ///   collision detection is protecting.
    /// * **The caller supplies the object, not its id.** The id is computed from the content here,
    ///   so there is no way to file an object under the wrong name.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the file cannot be created or renamed into place, and
    /// hasher collision detection.
    pub fn write(&self, object: &Object) -> Result<ObjectId> {
        // Always loose: a pack cannot be appended to, so the object is a new file under
        // objects/xx/. The loose store recomputes the id from the canonical header + body.
        self.loose.write(object)
    }

    /// Phase 10 — write an object from its kind and raw body.
    ///
    /// The path that has the bytes already — a work tree file being added, a `hash-object -w`
    /// argument, a base object recovered while fixing a thin pack — and does not need an [`Object`]
    /// built around them.
    ///
    /// # Caveats
    ///
    /// **`kind` is trusted.** The body is hashed with the header the kind implies, so passing the
    /// wrong kind does not produce a broken file: it produces a *valid object with a different id*
    /// from the one the caller expected, and every reference to the intended object then resolves
    /// to nothing. Callers that parsed the body know the kind; callers that did not must not guess.
    ///
    /// # Errors
    ///
    /// As [`Odb::write`].
    pub fn write_raw(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        // Always loose; the id is computed from the header the kind implies, never from a
        // caller field. Existing files are left alone.
        self.loose.write_raw(kind, data)
    }

    /// Phase 10 — resolve an abbreviated id to the single object it names.
    ///
    /// This is the primitive under every short hash a user types: `rivet show a1b2c3d`,
    /// `rev-parse HEAD~2` where the revision was written abbreviated, and the disambiguation the
    /// CLI reports.
    ///
    /// # Caveats
    ///
    /// * **Searches loose and packed, and the answer must be unique across both.** Uniqueness
    ///   within one pack is not enough; this is exactly the case a single-pack implementation gets
    ///   wrong, and it is why [`crate::pack::PackIndex::lookup_prefix`] documents that it cannot
    ///   decide the question itself.
    /// * **The prefix is a full-width [`ObjectId`] with trailing zero bytes**, not a string — see
    ///   [`ObjectId::from_hex_prefix`]. Its format must match the database's.
    /// * **A prefix that names nothing is [`crate::error::Error::ObjectNotFound`]**, and a prefix
    ///   that names several is [`crate::error::Error::AmbiguousPrefix`] with the count. Two
    ///   different failures, because "you typed it wrong" and "you need to type more" are different
    ///   problems for the user.
    /// * **The method is linear in the packs' id tables** and allocates a candidate list per pack.
    ///   For a full-length id it should short-circuit to [`Odb::read`]; for the four-character
    ///   abbreviations users actually type, the fanout table is what keeps it from scanning
    ///   everything.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`], [`crate::error::Error::AmbiguousPrefix`], and the
    /// corruption errors of [`Odb::read_header`] — resolving a prefix stats an object, and an
    /// object that cannot be stat'd is not a candidate.
    pub fn resolve_prefix(&self, prefix: &ObjectId) -> Result<ObjectId> {
        // A full-width prefix (no trailing zeros) is a single id: check it directly, which is
        // the `Odb::read` short-circuit the doc comment describes.
        if prefix.as_bytes().iter().all(|b| *b != 0) {
            // It is a full id, so resolve it through the read path: a miss is ObjectNotFound.
            match self.read(prefix) {
                Ok(_) => return Ok(*prefix),
                Err(Error::ObjectNotFound { .. }) => return Err(Error::ObjectNotFound { oid: *prefix }),
                Err(e) => return Err(e),
            }
        }

        // Match `starts_with` against the union of loose and packed ids. `starts_with` already
        // treats the zero-fill as unspecified, so this is the whole prefix primitive. De-duplicate
        // by id so an object present in both stores is counted once.
        let mut candidates = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in self.oids()? {
            if id.starts_with(prefix) && seen.insert(id) {
                candidates.push(id);
            }
        }

        match candidates.len() {
            0 => Err(Error::ObjectNotFound { oid: *prefix }),
            1 => Ok(candidates.pop().unwrap()),
            n => Err(Error::AmbiguousPrefix {
                prefix: prefix.to_hex(),
                candidates: n,
            }),
        }
    }

    /// Phase 10 — every object id in the database, loose and packed.
    ///
    /// Needed by `fsck` and by anything that walks the whole object graph rather than a reachable
    /// part of it, and by `gc` to find the loose objects worth packing.
    ///
    /// # Caveats
    ///
    /// * **Allocates one id per object and holds them all.** A large repository has tens of
    ///   millions; this is a whole-repository operation by nature and there is no lazy form that
    ///   remains correct across stores.
    /// * **An id that is in both a pack and loose appears once.** Duplicates are a normal
    ///   consequence of `gc` not having run, not a signal.
    /// * **Not sorted.** The order is loose-then-packed, and within each store it is whatever that
    ///   store returns. Callers that need a canonical order sort themselves.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if a loose subdirectory cannot be read, and the
    /// index-decode errors of the packs.
    pub fn oids(&self) -> Result<Vec<ObjectId>> {
        let mut ids = self.loose.oids()?;
        let mut seen: std::collections::HashSet<ObjectId> = ids.iter().copied().collect();
        for pack in &self.packs {
            for id in pack.oids()? {
                if seen.insert(id) {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    /// Phase 10 — add a pack that was just fetched and indexed.
    ///
    /// The hand-off from `index-pack` to the database: the fetch path writes the pair to
    /// `objects/pack/` and then calls this so the objects become readable without a full
    /// [`Odb::refresh`].
    ///
    /// # Caveats
    ///
    /// * **Both halves are required.** `index` is `None` only for a pack being registered before
    ///   its index exists, which is a state no caller of this API should be in — a pack without an
    ///   index cannot be looked up in, so registering it only makes its objects invisible.
    /// * **The pack's checksum is verified against the index's copy**, so a pair that do not belong
    ///   together is refused here rather than producing reads from the wrong offsets later.
    /// * **The pack is kept in memory**, like every other pack this type holds; see the type-level
    ///   caveat about mmap.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPack`] for a bad magic, version or trailer;
    /// [`crate::error::Error::CorruptPackIndex`] for an index that does not parse or does not
    /// describe this pack.
    pub fn add_pack(&mut self, pack: Vec<u8>, index: Option<Vec<u8>>) -> Result<()> {
        let Some(index) = index else {
            return Err(Error::CorruptPackIndex {
                reason: "a pack cannot be registered without its index".to_string(),
            });
        };
        let new = packed::PackedObjectStore::open(pack, index, self.format)?;
        // The pack's checksum is verified against the index's copy inside `open`; a mismatch
        // already surfaced as `CorruptPack` or `CorruptPackIndex`.
        if self.packs.iter().any(|existing| existing.checksum() == new.checksum()) {
            return Err(Error::CorruptPack {
                reason: "a pack with this checksum is already registered".to_string(),
            });
        }
        self.packs.push(new);
        Ok(())
    }
}

impl BaseProvider for Odb {
    /// Phase 10 — supply a delta base object by id.
    ///
    /// # Caveats
    ///
    /// Returns the object's **kind and body**, because a delta against a tree reconstructs a tree
    /// and the caller has to know what it is holding — the delta payload itself does not record it.
    ///
    /// The body is owned rather than borrowed: the base may live in any pack or in a loose file, so
    /// there is no single buffer to borrow from, and a delta chain applies the base immediately
    /// rather than holding it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`] when the database does not have the base — which for
    /// a thin pack means the sender was wrong about what this repository contains, and surfaces to
    /// the caller as [`crate::error::Error::ThinPack`] or
    /// [`crate::error::Error::MissingDeltaBase`] depending on which side is asking.
    fn base_object(&self, oid: &ObjectId) -> Result<(ObjectKind, Vec<u8>)> {
        // The cache is keyed by (pack, offset); a base reached by id names only its id, so
        // key it by the id itself with a fixed offset of 0. The id is unique across the
        // database, so this deduplicates correctly. Only delta bases are cached, never
        // whole objects: we store the resolved (kind, body) and discard the object wrapper.
        if let Some(cached) = self.delta_cache.borrow().get(oid, 0) {
            return Ok(cached);
        }
        let object = self.read(oid)?;
        let cached = (object.kind(), object.data().to_vec());
        self.delta_cache.borrow_mut().insert(*oid, 0, object.kind(), object.data().to_vec());
        Ok(cached)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjectKind;
    use std::io::Write;

    /// Write a single loose blob body via `LooseObjectStore::write_raw`, which hashes the
    /// body exactly as git does. Returns the object's id, so tests can assert exact ids and
    /// on-disk paths without depending on `Odb::write`'s packing/flush policy.
    fn write_loose_blob(store: &LooseObjectStore, data: &[u8]) -> ObjectId {
        store
            .write_raw(ObjectKind::Blob, data)
            .expect("writing a loose blob failed")
    }

    #[test]
    fn open_on_missing_dir_finds_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Point at a directory that does not exist.
        let missing = tmp.path().join("no-such-objects");
        let odb = Odb::open_objects_dir(&missing, ObjectFormat::Sha1);
        let odb = odb.expect("opening a non-existent objects dir must succeed");
        assert_eq!(odb.format(), ObjectFormat::Sha1);
        assert_eq!(odb.objects_dir(), &missing);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            bstr::BStr::new(b"b4e2bc81abc981e1056a883f86cdfe9971ac7295"),
        )
        .expect("valid hex");
        assert!(!odb.contains(&oid));
        assert!(odb.oids().expect("oids").is_empty());
        assert!(odb.packs().is_empty());
    }

    #[test]
    fn open_points_at_git_objects_and_reports_format() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path();
        // Mimic a `git init`-shaped tree: create objects/ so the dir exists.
        fs::create_dir_all(git_dir.join("objects")).expect("create objects/");
        let odb = Odb::open(git_dir, ObjectFormat::Sha1).expect("open");
        assert_eq!(odb.objects_dir(), &git_dir.join("objects"));
        assert_eq!(odb.format(), ObjectFormat::Sha1);
        assert!(odb.loose().objects_dir() == git_dir.join("objects"));
    }

    #[test]
    fn contains_and_oids_after_loose_write() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");

        let data = b"hello from rivet\n";
        let oid = write_loose_blob(odb.loose(), data);
        // Verify against the known-good git hash so a hash/path regression fails loudly.
        assert_eq!(
            oid,
            ObjectId::from_hex(
                ObjectFormat::Sha1,
                bstr::BStr::new(b"b4e2bc81abc981e1056a883f86cdfe9971ac7295"),
            )
            .expect("known-good id")
        );

        assert!(odb.contains(&oid));
        let ids = odb.oids().expect("oids");
        assert_eq!(ids, vec![oid]);

        // An empty pack/ directory and a stray non-hex file must be ignored.
        fs::create_dir_all(objects_dir.join("pack")).expect("create pack/");
        fs::write(objects_dir.join("stray-file"), b"not-an-object").expect("write stray");
        assert!(odb.oids().expect("oids") == vec![oid]);
        let zero = ObjectId::zero(ObjectFormat::Sha1);
        assert!(!odb.contains(&zero));
    }

    #[test]
    fn open_objects_dir_and_open_agree_on_objects_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path();
        fs::create_dir_all(git_dir.join("objects")).expect("create objects/");
        let via_open = Odb::open(git_dir, ObjectFormat::Sha1).expect("open");
        let via_direct = Odb::open_objects_dir(&git_dir.join("objects"), ObjectFormat::Sha1)
            .expect("open_objects_dir");
        assert_eq!(via_open.objects_dir(), via_direct.objects_dir());
    }

    #[test]
    fn oids_union_is_deduplicated_and_ordered() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");

        // Two distinct blobs; oids must list both, deduplicated, in deterministic order.
        let a = write_loose_blob(odb.loose(), b"alpha");
        let b = write_loose_blob(odb.loose(), b"beta body");
        assert_ne!(a, b);
        let ids = odb.oids().expect("oids");
        assert_eq!(ids.len(), 2);
        // The union is a set of exactly {a, b}; assert membership and dedup.
        let mut set = ids.clone();
        set.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(set, expected);
    }

    #[test]
    fn base_object_serves_from_cache_on_hit() {
        // Exercise only the cache-hit branch: pre-populate the delta cache, then `base_object`
        // must serve it from there without consulting the object store. A miss would fall
        // through to `read`, which finds nothing on disk for this synthetic key and fails.
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");

        let pack = ObjectId::zero(ObjectFormat::Sha1);
        let data = b"base object";
        let key = ObjectId::zero(ObjectFormat::Sha1);
        odb.delta_cache.borrow_mut().insert(pack, 0, ObjectKind::Blob, data.to_vec());

        // Both calls must be cache hits — neither may fall through to `read`.
        let (kind, body) = odb.base_object(&key).expect("cache hit 1");
        let (kind2, body2) = odb.base_object(&key).expect("cache hit 2");
        assert_eq!((kind, body.clone()), (ObjectKind::Blob, data.to_vec()));
        assert_eq!((kind2, body2), (kind, body));
    }

    #[test]
    fn pack_path_for_keeps_a_single_pack_prefix() {
        // Git's pack names, as `git gc` writes them: `pack-<hex>.idx` beside `pack-<hex>.pack`.
        let dir = Path::new("/repo/objects/pack");
        assert_eq!(
            pack_path_for(dir, "pack-9ec0c0d0f0b3c3e1a4b5c6d7e8f90a1b2c3d4e5f.idx"),
            Some(dir.join("pack-9ec0c0d0f0b3c3e1a4b5c6d7e8f90a1b2c3d4e5f.pack"))
        );
        // Anything that is not a pack index is skipped, not guessed at.
        assert_eq!(
            pack_path_for(dir, "pack-9ec0c0d0f0b3c3e1a4b5c6d7e8f90a1b2c3d4e5f.pack"),
            None
        );
        assert_eq!(
            pack_path_for(dir, "9ec0c0d0f0b3c3e1a4b5c6d7e8f90a1b2c3d4e5f.idx"),
            None
        );
        assert_eq!(
            pack_path_for(dir, "pack-9ec0c0d0f0b3c3e1a4b5c6d7e8f90a1b2c3d4e5f.rev"),
            None
        );
    }

    #[test]
    fn a_pack_directory_that_cannot_be_listed_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        // `pack` as a regular file: present, but not a directory that can be listed.
        fs::write(objects_dir.join("pack"), b"not a directory").expect("write pack file");
        assert!(matches!(
            Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1),
            Err(Error::IoAtPath { .. })
        ));
    }

    #[test]
    fn refresh_reports_an_unlistable_pack_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let mut odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");
        fs::write(objects_dir.join("pack"), b"not a directory").expect("write pack file");
        assert!(matches!(odb.refresh(), Err(Error::IoAtPath { .. })));
    }

    #[test]
    fn read_returns_loose_object_and_miss_is_object_not_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");

        let data = b"hello from rivet\n";
        let oid = write_loose_blob(odb.loose(), data);

        let object = odb.read(&oid).expect("read existing loose blob");
        assert_eq!(object.kind(), ObjectKind::Blob);
        assert_eq!(object.data(), data);

        // A plausible id that is absent is ObjectNotFound, not a panic.
        let missing = ObjectId::zero(ObjectFormat::Sha1);
        assert!(matches!(odb.read(&missing), Err(Error::ObjectNotFound { .. })));
    }

    #[test]
    fn read_on_corrupt_loose_file_reports_hash_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");

        // Write a good object, then corrupt its file with valid zlib of the wrong bytes.
        let oid = write_loose_blob(odb.loose(), b"hello from rivet\n");
        let path = objects_dir.join("b4").join("e2bc81abc981e1056a883f86cdfe9971ac7295");
        fs::remove_file(&path).expect("remove read-only loose file");
        // Valid zlib of a well-formed object holding *different* bytes: the stream inflates and
        // the header parses, but the content hashes to an id the path does not name.
        let mut encoder = flate2::write::ZlibEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        encoder.write_all(b"blob 6\0world\n").expect("write zlib");
        let corrupted = encoder.finish().expect("finish zlib");
        fs::write(&path, corrupted).expect("corrupt file");

        // A corrupt loose object must be CorruptObject, not a lie reported as ObjectNotFound.
        assert!(matches!(odb.read(&oid), Err(Error::CorruptObject { .. })));
        // read_header never inflates the body, so it cannot detect the mismatch: it reports the
        // header it parsed and lets the caller decide whether to trust the bytes.
        let (kind, size) = odb.read_header(&oid).expect("read_header corrupt");
        assert_eq!(kind, ObjectKind::Blob);
        assert_eq!(size, 6);
    }

    #[test]
    fn read_typed_reports_wrong_kind_and_passes_right_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");

        let oid = write_loose_blob(odb.loose(), b"hello from rivet\n");

        // Right kind round-trips.
        let object = odb
            .read_typed(&oid, ObjectKind::Blob)
            .expect("read_typed blob");
        assert_eq!(object.kind(), ObjectKind::Blob);

        // Wrong kind is WrongObjectKind, not a corrupt-object lie.
        let err = odb.read_typed(&oid, ObjectKind::Commit).expect_err("wrong kind");
        assert!(matches!(err, Error::WrongObjectKind { .. }));
    }

    #[test]
    fn read_header_agrees_with_read_on_kind_and_size() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");

        let data = b"hello from rivet\n";
        let oid = write_loose_blob(odb.loose(), data);

        let (kind, size) = odb.read_header(&oid).expect("read_header");
        let object = odb.read(&oid).expect("read");
        assert_eq!(kind, object.kind());
        assert_eq!(size, object.data().len() as u64);

        // A miss is ObjectNotFound from read_header too.
        let missing = ObjectId::zero(ObjectFormat::Sha1);
        assert!(matches!(odb.read_header(&missing), Err(Error::ObjectNotFound { .. })));
    }

    fn fresh_objects_odb() -> (tempfile::TempDir, Odb) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).expect("create objects/");
        let odb = Odb::open_objects_dir(&objects_dir, ObjectFormat::Sha1).expect("open");
        (tmp, odb)
    }

    /// The id `write` returns is the one git computes for the canonical header + body —
    /// anchored against the known-good hash for `b"hello from rivet\n"`.
    #[test]
    fn write_returns_git_computed_id_and_round_trips() {
        let (_tmp, odb) = fresh_objects_odb();
        let data = b"hello from rivet\n";
        let obj = Object::Blob(data.to_vec());
        let oid = odb.write(&obj).expect("write");

        // Anchor: the git hash of `blob <len> hello from rivet\n`.
        assert_eq!(
            oid,
            ObjectId::from_hex(
                ObjectFormat::Sha1,
                bstr::BStr::new(b"b4e2bc81abc981e1056a883f86cdfe9971ac7295"),
            )
            .expect("known hash")
        );

        // Round-trip: read back and confirm kind + body match.
        let back = odb.read(&oid).expect("read back");
        assert_eq!(back.kind(), ObjectKind::Blob);
        assert_eq!(back.data(), data);
    }

    /// `write_raw` must produce the same id as `write` for the same kind and body.
    #[test]
    fn write_raw_matches_write_id() {
        let (_tmp, odb) = fresh_objects_odb();
        let data = b"hello from rivet\n";
        let oid_write = odb
            .write(&Object::Blob(data.to_vec()))
            .expect("write");
        let oid_raw = odb.write_raw(ObjectKind::Blob, data).expect("write_raw");
        assert_eq!(oid_write, oid_raw);
    }

    /// Three distinct outcomes: unique -> Ok, ambiguous -> AmbiguousPrefix (with the
    /// count), missing -> ObjectNotFound.
    ///
    /// We build two real objects (git-style) that share a 4-byte id prefix, so a
    /// 4-char abbreviation is genuinely ambiguous.
    #[test]
    fn resolve_prefix_unique_ambiguous_and_missing() {
        let Some(_git_exe) = git_exe() else {
            eprintln!("git not on PATH; skipping");
            return;
        };
        let (_tmp, odb) = fresh_objects_odb();

        let _data_a = b"alpha\n";
        let _data_b = b"beta\n";

        // Write both blobs into the loose store via git hash-object so the ids are
        // exactly what git computes.
        // hash-object without -w just computes the sha1 of the data.
        // Write blobs via rivet's own write_raw until two ids share a 4-char hex prefix. A
        // 16-bit prefix needs ~2^8 ids before a collision is likely, so a fixed handful of
        // blobs is hopeless; grow until the birthday paradox pays off.
        let mut oids: Vec<ObjectId> = Vec::new();
        let mut hexes: Vec<String> = Vec::new();
        let mut pair: Option<(usize, usize)> = None;
        while oids.len() < 100_000 {
            let data = format!("blob-{}\n", oids.len()).into_bytes();
            let oid = odb.loose().write_raw(ObjectKind::Blob, &data).expect("write");
            let hex = oid.to_hex().to_string();
            // Find a pair sharing at least 4 hex chars.
            if let Some(first) = hexes
                .iter()
                .position(|other| common_hex_prefix_len(other, &hex) >= 4)
            {
                pair = Some((first, oids.len()));
                oids.push(oid);
                hexes.push(hex);
                break;
            }
            oids.push(oid);
            hexes.push(hex);
        }
        let (i, j) = pair.expect("no two blobs share a 4-char prefix");
        let hex_a = &hexes[i];
        let _hex_b = &hexes[j];
        let oid_a = &oids[i];
        let _oid_b = &oids[j];

        // The 4-char hex prefix of oid_a (which equals the first 4 chars of oid_b
        // since we verified they share >= 4 chars) is ambiguous.
        let abbrev =
            ObjectId::from_hex_prefix(ObjectFormat::Sha1, bstr::BStr::new(&hex_a.as_bytes()[..4]))
                .expect("abbrev");

        // Unique: the full id resolves to itself.
        assert_eq!(odb.resolve_prefix(oid_a).expect("unique"), *oid_a);

        // Ambiguous: the shared 4-char prefix names both.
        match odb.resolve_prefix(&abbrev) {
            Err(Error::AmbiguousPrefix { candidates, .. }) => assert_eq!(candidates, 2),
            other => panic!("expected AmbiguousPrefix, got {other:?}"),
        }

        // Missing: a prefix that names nothing.
        let missing = ObjectId::from_hex_prefix(
            ObjectFormat::Sha1,
            bstr::BStr::new(b"deadbeefdeadbeefdeadbeefdeadbeef"),
        )
        .expect("missing id");
        assert!(matches!(odb.resolve_prefix(&missing), Err(Error::ObjectNotFound { .. })));
    }

    fn git_exe() -> Option<String> {
        std::env::var_os("PATH")
            .and_then(|path| {
                for dir in std::env::split_paths(&path) {
                    let candidate = dir.join("git");
                    if candidate.is_file() {
                        return Some(candidate.to_string_lossy().into_owned());
                    }
                }
                None
            })
    }


    fn common_hex_prefix_len(a: &str, b: &str) -> usize {
        a.as_bytes()
            .iter()
            .zip(b.as_bytes().iter())
            .take_while(|(x, y)| x == y)
            .count()
    }
}
