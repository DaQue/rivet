//! Phase 2 — `packed-refs`: every ref that is not a loose file, in one file.
//!
//! Git keeps each ref as its own file until there are enough of them to be a nuisance — a few
//! thousand branches, or a clone of a repository with a tag per release — and then `git pack-refs`
//! moves them all into `.git/packed-refs`. The loose file is deleted at that point, so a reader
//! that only looks in `refs/` sees a repository with no tags in it, which is the single most
//! common way a hand-written ref reader goes wrong.
//!
//! The format is line-oriented and has no checksum:
//!
//! ```text
//! # pack-refs with: peeled fully-peeled sorted
//! 3f2c1a… refs/heads/main
//! 7b1e0d… refs/tags/v1.0
//! ^9a4f88…            <- the commit v1.0's tag object peels to
//! ```
//!
//! # Caveats that bite
//!
//! * **A loose file shadows the packed entry.** `pack-refs` does not remove a ref from the file
//!   when it is later updated; the update writes a loose file instead, and the packed entry stays
//!   behind as a stale shadow. Reading packed-refs without checking loose first returns the *old*
//!   value, which is why every lookup in [`crate::refs::RefStore`] tries loose first. The stale
//!   entry is not a bug to be cleaned up eagerly: removing it requires rewriting the whole file,
//!   and Git leaves it until the next `pack-refs` run.
//! * **A peeled line is an optimisation, not information.** `^<id>` records what a tag resolves
//!   to, saving a tag-object read. If it is absent — or if the file was written without the
//!   `fully-peeled` trait, so its absence means "unknown" rather than "not a tag" — it must be
//!   recomputed by reading the object. A reader that treats a missing peel line as "peels to
//!   nothing" will silently fail to resolve annotated tags.
//! * **The header's traits change how to read the file.** `sorted` promises the entries are in
//!   byte order (so a binary search is valid); `fully-peeled` promises that every tag that can be
//!   peeled has a `^` line. Neither trait being present is normal for files written by older Git,
//!   and assuming them on a file that lacks them produces wrong answers rather than errors.
//! * **The file has no checksum**, unlike the index and packs. A truncated `packed-refs` — the
//!   result of a killed `gc` that was not using the lock file — parses as a shorter list of refs,
//!   and the only defence is that the writer renames into place atomically.
//! * **The header line Git writes ends with a trailing space** when there are traits, and the
//!   trait list is space-separated: `# pack-refs with: peeled fully-peeled sorted `. A parser that
//!   splits on whitespace gets a trailing empty token; a serialiser that omits the trailing space
//!   produces a file that differs from Git's byte-for-byte for no benefit.
//! * **Peeled lines follow their ref immediately** and are meaningless on their own. A `^` line
//!   before any ref line is corruption, not an entry without a name.

use std::path::{Path, PathBuf};

use bstr::BString;

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};

use super::RefName;

/// Phase 2 — the literal start of `packed-refs`' header line.
///
/// # Caveats
///
/// The header is optional in the sense that the file is readable without it, and Git has written
/// files with and without it. Its presence is a promise about the entries that follow, so it must
/// be parsed rather than skipped; its absence means "no promises", which is why the traits are
/// tracked per file rather than assumed.
pub const HEADER_PREFIX: &[u8] = b"# pack-refs with:";

/// Phase 2 — the trait strings Git may write in the header.
///
/// # Caveats
///
/// These are not configuration and not version numbers; they are capabilities of the file's
/// contents. An unknown trait must be preserved verbatim on a rewrite (so a newer Git's file
/// survives a round trip through Rivet) rather than dropped.
pub mod traits {
    /// The entries are sorted by ref name in byte order, so a binary search is valid.
    pub const SORTED: &[u8] = b"sorted";
    /// Every ref that points at a tag object has a `^` peel line.
    pub const PEELED: &[u8] = b"peeled";
    /// The `peeled` promise holds for *every* tag in the file, so a tag with no `^` line is a
    /// lightweight tag rather than one whose peel is unknown.
    pub const FULLY_PEELED: &[u8] = b"fully-peeled";
}

/// Phase 2 — one entry of a `packed-refs` file.
///
/// # Caveats
///
/// [`PackedRef::peeled`] is `None` in two situations that mean different things: a lightweight tag
/// (nothing to peel) and an unpeeled annotated tag whose `^` line the writer omitted. Telling them
/// apart requires the file's `fully-peeled` trait — see [`PackedRefs::is_fully_peeled`] — or an
/// object read that resolves the tag for real. Code that needs a commit id from a tag must cope
/// with `None` by reading the object, never by treating it as "the tag is already a commit".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedRef {
    /// The full ref name, e.g. `refs/tags/v1.0`.
    pub name: RefName,
    /// What the ref directly points at, which for an annotated tag is the tag object.
    pub target: ObjectId,
    /// What the target peels to, from the `^` line that followed this entry.
    pub peeled: Option<ObjectId>,
}

/// Phase 2 — a parsed `packed-refs` file.
///
/// # Caveats
///
/// * **Entry order is the file's order**, which is byte-sorted for any file Git wrote with the
///   `sorted` trait and arbitrary otherwise. [`PackedRefs::get`] therefore does a linear scan
///   unless the trait is present, and a caller that wants a deterministic listing must call
///   [`PackedRefs::sort`] rather than relying on the file.
/// * **The header traits are preserved**, including ones Rivet does not recognise, so that a
///   read-modify-write cycle does not silently downgrade a file a newer Git wrote. This is the
///   same rule as unknown commit headers: an unrecognised field is data to carry, not noise to
///   drop.
/// * **An empty file and a missing file are the same value.** Both mean "no packed refs", which is
///   the state of a repository that has never run `pack-refs` and is not an error.
/// * **Not a cache of ref values.** It is parsed at [`crate::refs::RefStore::open`] and re-read on
///   [`crate::refs::RefStore::refresh`], because a concurrent `git gc` can rewrite it at any time
///   and a stale `PackedRefs` produces reads that succeed and return the wrong commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedRefs {
    /// Trait strings from the header line, in file order. Empty when the header was absent.
    traits: Vec<BString>,
    /// The entries, in file order.
    entries: Vec<PackedRef>,
}

impl PackedRefs {
    /// Phase 2 — an empty set, as if from a repository that has never packed its refs.
    pub fn new() -> Self {
        Self {
            traits: Vec::new(),
            entries: Vec::new(),
        }
    }

    /// Phase 2 — read `git_dir/packed-refs`.
    ///
    /// # Caveats
    ///
    /// **A missing file is not an error** — it is the normal state of a fresh or actively-edited
    /// repository, and the result is an empty set. A file that exists and does not parse *is* an
    /// error, because the alternative is reporting a branch as missing and then having `gc` write
    /// a `packed-refs` without it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the file exists but cannot be read, and
    /// [`crate::error::Error::CorruptPackedRefs`] for a line that is not a comment, a blank, a
    /// peel, or a well-formed `<id> <name>` pair.
    pub fn load(git_dir: &Path, format: ObjectFormat) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — parse the file's bytes.
    ///
    /// # Caveats
    ///
    /// * Lines are split on `\n`, and a trailing `\r` must be stripped so that a file checked out
    ///   on Windows with `core.autocrlf` — which Git does *not* do for this file, but hand-edited
    ///   files in the wild do — still parses. A ref name with a trailing `\r` would be a different
    ///   ref, not an error.
    /// * The ref name is taken **verbatim to the end of the line**, so a name containing a space is
    ///   preserved; the split is on the *first* space only. Refs with spaces are legal (though
    ///   awkward) and splitting on all whitespace truncates them.
    /// * A duplicate name later in the file wins, matching the "last match wins" behaviour of a
    ///   shadowed loose ref. Git should never write one; a hand-edited file can.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPackedRefs`] carrying the 1-based line number; also
    /// [`crate::error::Error::InvalidRefName`] for a line whose name field is not a valid ref name,
    /// which is reported rather than skipped because a skipped ref is a ref that goes missing on
    /// the next write.
    pub fn parse(format: ObjectFormat, data: &[u8]) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — serialise back to the file's bytes.
    ///
    /// # Caveats
    ///
    /// * The header line is written only when there is at least one trait, with a **trailing space**
    ///   after the last one, matching Git byte-for-byte. A file with no traits gets no header rather
    ///   than an empty one, because `# pack-refs with:` followed by nothing is a header Git never
    ///   writes and that a strict reader may reject.
    /// * Peel lines are written immediately after their ref, and only for entries that have one —
    ///   so a `fully-peeled` file must not have its `peeled` trait claimed by a writer that is not
    ///   actually emitting every peel. [`PackedRefs::write`] is responsible for deciding the traits
    ///   it can honestly promise, and must not simply echo the ones it read if its contents changed.
    /// * Ids are written at the repository's format width, lowercase hex. There is no format field
    ///   in the file: a SHA-256 repository's `packed-refs` is distinguishable only by the line
    ///   length, which is exactly why the caller supplies the format.
    pub fn serialize(&self, format: ObjectFormat) -> Vec<u8> {
        todo!()
    }

    /// Phase 2 — write `git_dir/packed-refs` atomically.
    ///
    /// # Caveats
    ///
    /// * **The lock file is `packed-refs.lock`**, taken and renamed into place through
    ///   [`crate::refs::RefLock`]. A concurrent `git gc` holds the same lock, and this is the only
    ///   thing that keeps two writers from interleaving their rewrites.
    /// * **Loose refs are untouched.** Writing a packed-refs that shadows a loose file is legal and
    ///   is what happens when a caller packs a subset; the loose value continues to win on read.
    /// * **The file is rewritten whole**, so its size is a function of the ref count and there is no
    ///   incremental form. That is why this is a `gc` operation and not something every ref update
    ///   does.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`] if `packed-refs.lock` exists, and
    /// [`crate::error::Error::IoAtPath`] if the file cannot be written or renamed into place.
    pub fn write(&self, git_dir: &Path, format: ObjectFormat) -> Result<()> {
        todo!()
    }

    /// Phase 2 — the target of a ref, if it is packed.
    ///
    /// # Caveats
    ///
    /// **This is not the ref's value.** A loose file with the same name shadows this entry, so a
    /// caller asking "what is `refs/heads/main`" must consult loose storage first — which is what
    /// [`crate::refs::RefStore::read`] does and why this method is on [`PackedRefs`] rather than on
    /// the store as the whole answer.
    pub fn get(&self, name: &RefName) -> Option<ObjectId> {
        todo!()
    }

    /// Phase 2 — the peeled target recorded for a ref, if the file has one.
    ///
    /// # Caveats
    ///
    /// `None` does not mean "not a tag"; see [`PackedRef::peeled`]. The `fully-peeled` trait is what
    /// makes `None` decisive.
    pub fn peeled(&self, name: &RefName) -> Option<ObjectId> {
        self.get(name)
    }

    /// Phase 2 — whether this file's entries are sorted by name.
    ///
    /// # Caveats
    ///
    /// Read from the header trait, not verified against the entries. A file that claims `sorted` and
    /// is not sorted is corrupt in a way Rivet does not detect, because checking would cost a scan
    /// of every ref on every open — and Git does not check either.
    pub fn is_sorted(&self) -> bool {
        todo!()
    }

    /// Phase 2 — whether every peelable tag in this file has a peel line.
    ///
    /// # Caveats
    ///
    /// This is the trait that makes a missing [`PackedRef::peeled`] meaningful. Without it, a tag
    /// entry with no `^` line may be either a lightweight tag or an annotated one whose peel the
    /// writer chose not to record — and the only way to tell is to read the tag object.
    pub fn is_fully_peeled(&self) -> bool {
        todo!()
    }

    /// Phase 2 — the header trait strings, in file order.
    pub fn traits(&self) -> &[BString] {
        &self.traits
    }

    /// Phase 2 — add or replace the trait strings written in the header.
    ///
    /// # Caveats
    ///
    /// **This is a claim about the file's contents, and a false one is worse than none.** A writer
    /// that sets [`traits::FULLY_PEELED`] without having resolved every tag's peel makes subsequent
    /// reads report lightweight tags where there are annotated ones. The only honest way to set it
    /// is to have peeled every tag while building the file.
    pub fn set_traits(&mut self, traits: Vec<BString>) {
        todo!()
    }

    /// Phase 2 — the entries, in file order.
    pub fn entries(&self) -> &[PackedRef] {
        todo!()
    }

    /// Phase 2 — iterate the entries in file order.
    ///
    /// # Caveats
    ///
    /// The order is the file's, so it is byte-sorted only when the `sorted` trait is present. Use
    /// [`PackedRefs::sort`] first, or [`PackedRefs::get`], if the order matters.
    pub fn iter(&self) -> std::slice::Iter<'_, PackedRef> {
        todo!()
    }

    /// Phase 2 — the ref names, in file order.
    ///
    /// # Caveats
    ///
    /// Borrowing the names rather than allocating, which is what the free function
    /// [`packed_names`] is for when an owned list is wanted. The order is the file's.
    pub fn names(&self) -> PackedRefNames<'_> {
        todo!()
    }

    /// Phase 2 — how many refs are packed.
    pub fn len(&self) -> usize {
        todo!()
    }

    /// Phase 2 — whether nothing is packed.
    pub fn is_empty(&self) -> bool {
        todo!()
    }

    /// Phase 2 — insert an entry, replacing any entry with the same name.
    ///
    /// # Caveats
    ///
    /// * **The sorted trait is not maintained for you.** The entry is appended when the name is new,
    ///   so calling this on a set that has the trait set leaves the file claiming an order it no
    ///   longer has. A caller building a sorted file must call [`PackedRefs::sort`] before
    ///   [`PackedRefs::write`], and must set the trait explicitly.
    /// * Replacing an entry **drops the old peel**, because a new target peels to something else.
    ///   Leaving the stale `^` line would resolve the tag to a commit it no longer points at.
    /// * Returns the entry that was replaced, so a caller can report what it overwrote.
    pub fn insert(&mut self, entry: PackedRef) -> Option<PackedRef> {
        todo!()
    }

    /// Phase 2 — remove an entry by name.
    ///
    /// # Caveats
    ///
    /// **Removing a packed entry does not make the ref disappear** if a loose file with the same
    /// name exists — that loose file is the ref. This is the operation `gc` performs on entries it
    /// has just written as loose, which is why it returns the removed entry rather than a `bool`:
    /// the caller wants to know what it just deleted.
    pub fn remove(&mut self, name: &RefName) -> Option<PackedRef> {
        todo!()
    }

    /// Phase 2 — drop every entry for which the predicate is false.
    ///
    /// The operation behind `pack-refs --prune` and behind removing refs that now exist as loose
    /// files. Like [`PackedRefs::remove`], it does not touch loose storage.
    pub fn retain(&mut self, keep: impl FnMut(&PackedRef) -> bool) {
        todo!()
    }

    /// Phase 2 — record what a ref's target peels to.
    ///
    /// # Caveats
    ///
    /// A no-op when the name is not present, because a peel line with no ref to attach to is
    /// corruption. Callers that need to know whether the peel landed must check
    /// [`PackedRefs::get`] first.
    pub fn set_peeled(&mut self, name: &RefName, peeled: ObjectId) {
        todo!()
    }

    /// Phase 2 — sort the entries by ref name in byte order.
    ///
    /// # Caveats
    ///
    /// **Byte order, not locale order.** Git's `sorted` trait means `memcmp` order on the raw name
    /// bytes, which is not the order a user sees under `versionsort` and not the order of any
    /// locale's collation. Sorting any other way produces a file whose `sorted` claim is a lie and
    /// whose binary search returns the wrong entry.
    pub fn sort(&mut self) {
        todo!()
    }
}

/// Phase 2 — the ref names in a [`PackedRefs`], yielded in file order.
///
/// # Caveats
///
/// * **This is a named iterator because a scaffold cannot return `impl Iterator`.** Every function
///   body in this crate is `todo!()`, and a `todo!()` body leaves the compiler unable to write the
///   hidden type that an `impl Trait` return would describe. A struct is a type, so the signature
///   holds. The implementation is a thin wrapper over `slice::Iter` plus a field projection.
/// * **Borrowed from the set**, so the names are valid only while it is; use [`packed_names`] for an
///   owned vector.
/// * **The order is the file's**, not sorted — see [`PackedRefs::iter`].
#[derive(Clone, Debug)]
pub struct PackedRefNames<'a> {
    /// The set being iterated.
    inner: &'a PackedRefs,
    /// How far into [`PackedRefs::entries`] the iterator has reached.
    position: usize,
}

impl<'a> Iterator for PackedRefNames<'a> {
    type Item = &'a RefName;

    /// Phase 2 — the next name, or `None` at the end.
    ///
    /// # Panics
    ///
    /// Scaffold stage: this body is `todo!()`. The implementation must read
    /// `self.inner.entries.get(self.position)` and advance `self.position`, and must not index
    /// directly — an index past the end would panic on a `packed-refs` that shrank mid-iteration.
    fn next(&mut self) -> Option<Self::Item> {
        todo!()
    }

    /// Phase 2 — how many names are left.
    ///
    /// # Panics
    ///
    /// Scaffold stage: this body is `todo!()`.
    fn size_hint(&self) -> (usize, Option<usize>) {
        todo!()
    }
}

impl Default for PackedRefs {
    /// Phase 2 — an empty set; identical to [`PackedRefs::new`].
    ///
    /// # Panics
    ///
    /// Scaffold stage: this body is `todo!()`. The implementation must simply return
    /// [`PackedRefs::new`]'s value.
    fn default() -> Self {
        todo!()
    }
}

/// Phase 2 — the path of the packed-refs file inside a git directory.
///
/// # Caveats
///
/// Provided so that writers and `gc` agree on one spelling. It is *not* `refs/packed-refs` —
/// the file sits at the top of the git directory, beside `HEAD` and `config`.
pub fn packed_refs_path(git_dir: &Path) -> PathBuf {
    todo!()
}

/// Phase 2 — whether a ref exists as a loose file, in `packed-refs`, or both.
///
/// # Caveats
///
/// * [`RefLocation::Both`] is not an error and not a transient state to be resolved: it is what
///   every repository looks like after a ref is updated following a `pack-refs`, and it can
///   persist indefinitely.
/// * This reports *storage*, not value. Two locations can hold different ids — that is the point of
///   loose shadowing packed — so a caller that wants the ref must read it, not this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefLocation {
    /// Only a loose file under `refs/` (or a pseudo-ref at the top of the git directory).
    Loose,
    /// Only an entry in `packed-refs`.
    Packed,
    /// A loose file *and* a packed entry. The loose file wins on read.
    Both,
}

impl RefLocation {
    /// Phase 2 — whether a loose file is part of this ref's storage.
    pub const fn has_loose(self) -> bool {
        todo!()
    }

    /// Phase 2 — whether a `packed-refs` entry is part of this ref's storage.
    pub const fn has_packed(self) -> bool {
        todo!()
    }
}

/// Phase 2 — work out where a ref is stored, given both halves.
///
/// # Caveats
///
/// Takes the two answers rather than doing the lookups itself, so that a caller that has already
/// stat'd the file and parsed the packed set does not pay for it twice. `None` for both is not
/// representable: a ref that is nowhere has no location, and a caller in that position has a
/// [`crate::error::Error::RefNotFound`] to raise instead.
pub fn location(loose_present: bool, packed_present: bool) -> RefLocation {
    todo!()
}

/// Phase 2 — the full names of every ref in a `PackedRefs` set, in file order.
///
/// # Caveats
///
/// A convenience over [`PackedRefs::names`] for callers that need an owned list — `gc` and the
/// listing behind `rivet branch -r`. It allocates one [`RefName`] per entry, which for a repository
/// with a tag per release is tens of thousands; a caller that only needs to iterate should use
/// [`PackedRefs::iter`].
pub fn packed_names(packed: &PackedRefs) -> Vec<RefName> {
    todo!()
}
