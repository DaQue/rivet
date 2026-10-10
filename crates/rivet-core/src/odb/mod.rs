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
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{Object, ObjectKind};
use crate::pack::BaseProvider;

pub mod loose;
pub mod packed;

use packed::DeltaBaseCache;

pub use loose::LooseObjectStore;
pub use packed::PackedObjectStore;

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
    /// The `objects` directory. Loose objects live in two-hex subdirectories of it and packs in
    /// `pack/`; the path is kept so a written object can be placed without rescanning.
    objects_dir: PathBuf,
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
        todo!()
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
        todo!()
    }

    /// Phase 10 — the `objects` directory this database is over.
    pub fn objects_dir(&self) -> &Path {
        todo!()
    }

    /// Phase 10 — the repository's id width, as it was supplied at open.
    ///
    /// # Caveats
    ///
    /// Returned as given, not detected. A mismatch with the objects on disk is not detectable here
    /// and produces wrong reads rather than errors — the one place it is caught is the final hash
    /// check, which is a [`crate::error::Error::HashMismatch`] that looks like corruption.
    pub fn format(&self) -> ObjectFormat {
        todo!()
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
        todo!()
    }

    /// Phase 10 — the packs this database knows about, in discovery order.
    ///
    /// # Caveats
    ///
    /// The order is the filesystem's, so it is **not stable across machines** and must not be
    /// relied on for anything but reporting. A prefix that appears in two packs is ambiguous
    /// regardless of this order, and [`Odb::resolve_prefix`] says so rather than picking the first.
    pub fn packs(&self) -> &[PackedObjectStore] {
        todo!()
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
        todo!()
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
        todo!()
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
        todo!()
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
        todo!()
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
        todo!()
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
        todo!()
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
        todo!()
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
    /// For a full-length id it should short-circuit to [`Odb::read`]; for the four-character
    /// abbreviations users actually type, the fanout table is what keeps it from scanning
    /// everything.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ObjectNotFound`], [`crate::error::Error::AmbiguousPrefix`], and the
    /// corruption errors of [`Odb::read_header`] — resolving a prefix stats an object, and an
    /// object that cannot be stat'd is not a candidate.
    pub fn resolve_prefix(&self, prefix: &ObjectId) -> Result<ObjectId> {
        todo!()
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
        todo!()
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
        todo!()
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
        todo!()
    }
}
