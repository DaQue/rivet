//! Phase 2 — refs: names that point at commits, and the machinery that moves them safely.
//!
//! A ref is a name (`refs/heads/main`) with a value: an object id, or a redirect to another ref.
//! Between them they are how a repository remembers anything — `HEAD` remembers where you are,
//! branches remember where work is, tags remember what a release was, and remote-tracking refs
//! remember what the last fetch saw.
//!
//! # The storage layout
//!
//! ```text
//! .git/HEAD                     ref: refs/heads/main      (or a detached id)
//! .git/refs/heads/main          3f2c1a9e…                 (a loose ref)
//! .git/packed-refs              3f2c1a9e… refs/heads/main  (many refs, one file)
//! .git/logs/refs/heads/main     0000000… 3f2c1a9e… … \t commit: …   (the reflog)
//! .git/refs/heads/main.lock     (held while updating)
//! ```
//!
//! # The four rules that a ref reader must not break
//!
//! 1. **A loose file shadows a packed entry**, always. `git pack-refs` deletes the loose files it
//!    absorbs, but an update after that writes a *new* loose file and leaves the packed entry
//!    behind as a stale shadow. Reading packed-refs first returns an old commit, silently — the
//!    single most common way a from-scratch ref reader is wrong. [`RefStore::read`] tries loose,
//!    then packed.
//! 2. **Every update is `<ref>.lock` then rename.** Creating the lock is what makes two updaters
//!    mutual-exclude, and the rename is what makes a reader see one value or the other rather than
//!    a truncated file. A stale lock left by a crash is indistinguishable from a live one, so it is
//!    never removed automatically ([`crate::error::Error::LockHeld`] names the file for the user).
//! 3. **A symbolic ref is followed with a bound.** `HEAD` normally holds `ref: refs/heads/main`
//!    rather than an id, and a chain of symbolic refs is legal — but a cycle is not, and a hostile
//!    repository can describe one. The walk stops at [`MAX_SYMBOLIC_DEPTH`] and reports
//!    [`crate::error::Error::SymbolicRefCycle`] rather than recursing until the stack overflows
//!    (plan §5).
//! 4. **The object format is the repository's, not the string's.** Every id written here is 20 or
//!    32 bytes according to `extensions.objectFormat`, and every id parsed here is checked against
//!    that width. A ref file is not self-describing: it is `refs/heads/main` either way.
//!
//! # Caveats that bite
//!
//! * **`HEAD` is a ref, not a special case.** It can be symbolic or direct (detached), it has a
//!    reflog at `logs/HEAD`, and it is locked and updated like any other. It is one of the
//!    one-component pseudo-refs, so [`RefName::parse`] accepts `HEAD` while rejecting a bare
//!    `main`.
//! * **A ref does not have to point at a commit.** `refs/tags/v1.0` may name an annotated tag
//!    object, and a tag points at whatever it was tagged. Resolving a ref to a commit means peeling
//!    tags first, which is revision parsing (Phase 4) and needs the object database; this module
//!    deliberately stops at "what does the ref file say". [`RefStore::head_commit`] is the one
//!    helper that takes the extra step, because everything the CLI does needs it.
//! * **Refs are not a snapshot.** A concurrent `git gc`, `commit` or `fetch` can move a ref between
//!    two reads of the same store, and a `pack-refs` run can move a ref from a file into
//!    `packed-refs` under a reader's feet. A long-lived [`RefStore`] must [`RefStore::refresh`],
//!    especially after an operation that failed to find a ref that the user can see.
//! * **Deleting a ref is two deletions.** The loose file and the `packed-refs` entry are separate
//!    pieces of storage, and a deletion that removes only one leaves the ref in place. Git's own
//!    delete handles both; so must [`RefStore::delete`].
//! * **Nothing here allocates a lock implicitly at open.** A [`RefStore`] holds no file handles and
//!    takes no locks; a lock exists only for the duration of [`RefLock`]'s guard, which is what
//!    makes an interrupted process leave a stale lock rather than a half-written ref.
//! * **`refs/` is walked, so its contents are untrusted.** Loose ref storage is a directory tree on
//!    a filesystem the user (and malware) can write to: a symlink under `refs/` must not be
//!    followed, a file whose name is not a valid ref name must be skipped rather than failing the
//!    listing, and a ref file is bounded in size rather than read into memory whatever its length.

pub mod loose;
pub mod name;
pub mod packed;
pub mod reflog;

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::ObjectKind;

pub use name::{HEAD, RefCategory, RefName};
pub use packed::{PackedRef, PackedRefs, RefLocation, packed_refs_path};
pub use reflog::{ReflogEntry, ReflogMessage};

/// Phase 2 — how many symbolic refs deep Rivet will follow before giving up.
///
/// # Caveats
///
/// Five, matching Git's own `SYMREF_MAXDEPTH`. The limit is not about real repositories — `HEAD` to
/// a branch is one hop and nothing uses more than two — it is about the cycle case: `A` pointing at
/// `B` pointing at `A` never terminates, and a resolver that recursed without a bound would abort
/// the process rather than report [`crate::error::Error::SymbolicRefCycle`] (plan §5). Raising it
/// is safe; removing it is not.
pub const MAX_SYMBOLIC_DEPTH: usize = 5;

/// Phase 2 — what a ref holds.
///
/// # Caveats
///
/// * **[`RefTarget::Symbolic`] is not resolved.** It records what the ref file *says*, which for
///   `HEAD` is normally a redirect. [`RefStore::read`] returns this form, and only
///   [`RefStore::resolve_oid`] follows it — so a caller handing a `Symbolic` target to something
///   that expects an object id will not compile, which is the point.
/// * **A symbolic target may name a ref that does not exist.** That is the state of a repository
///   with `HEAD: refs/heads/main` and no `main` branch yet — an unborn branch, which `git init`
///   leaves behind and which `git status` reports as "No commits yet on main". It is legal, and it
///   means resolution has to distinguish "the chain ended at a missing ref" from "the chain
///   cycles".
/// * **A direct target may be a tag object.** Nothing here peels; see the module caveat.
/// * The bytes of a ref file are not preserved, unlike a commit's `raw`. A ref file's whole content
///   is one of these two forms, so re-serialising loses nothing — with the single exception of
///   whitespace tolerance, which is not information worth round-tripping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefTarget {
    /// A ref that names an object id directly.
    Direct(ObjectId),
    /// A ref that redirects to another ref, e.g. `HEAD` holding `ref: refs/heads/main`.
    Symbolic(RefName),
}

impl RefTarget {
    /// Phase 2 — the object id, requiring the target to be direct.
    ///
    /// # Caveats
    ///
    /// Symbolic targets are [`crate::error::Error::SymbolicRef`] rather than followed implicitly.
    /// A caller that wants the resolved value uses [`RefStore::resolve_oid`], which can see the
    /// rest of the repository; a function here that half-resolved would be the place an unborn
    /// branch turned into a stale id.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::SymbolicRef`] if the target is symbolic.
    pub fn oid(&self) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 2 — the symbolic target's name, requiring the target to be symbolic.
    ///
    /// The mirror of [`RefTarget::oid`], for the `symbolic-ref` path.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::SymbolicRef`] if the target is direct.
    pub fn symbolic_target(&self) -> Result<&RefName> {
        todo!()
    }

    /// Phase 2 — whether this target redirects to another ref.
    pub const fn is_symbolic(&self) -> bool {
        todo!()
    }

    /// Phase 2 — whether this target names an object id.
    ///
    /// # Caveats
    ///
    /// The negation of [`RefTarget::is_symbolic`], and not the same as "resolvable": a direct
    /// target names an id that may or may not be in the object database at all.
    pub const fn is_direct(&self) -> bool {
        todo!()
    }

    /// Phase 2 — serialise to the exact bytes of a loose ref file, **including** the trailing
    /// newline.
    ///
    /// # Caveats
    ///
    /// * The `ref: ` prefix is exactly `"ref: "` — one space — because that is what Git's reader
    ///   matches, and a file with any other spelling is one Git treats as corrupt. Getting this
    ///   wrong produces refs that Rivet can read and Git cannot.
    /// * The hex id is lowercase and at the repository's format width, which is why the format is a
    ///   parameter even though the value carries its own — a mismatch is a caller bug that this has
    ///   no way to detect at the type level.
    /// * The newline is included because every caller is writing a file; a caller that wants the
    ///   value without it strips one byte.
    pub fn to_bytes(&self, format: ObjectFormat) -> Vec<u8> {
        todo!()
    }
}

/// Phase 2 — the lock file guard that makes a ref update atomic against other processes.
///
/// Constructing one creates `<ref>.lock` with `O_CREAT | O_EXCL`, which is the whole of Git's
/// mutual exclusion. [`RefLock::commit`] renames the lock file over the ref; dropping the guard
/// without committing removes it. Either way the lock is released, so a guard that lives until the
/// end of a scope holds the ref for that scope and no longer.
///
/// # Caveats
///
/// * **A lock is not a mutex.** It excludes other *processes* that respect it — every `git`
///   invocation does — but two threads in one process that each construct a `RefLock` for the same
///   ref both succeed on the filesystem and both then write. A caller sharing a `RefStore` between
///   threads must serialise ref updates itself.
/// * **Stale locks are not cleaned up.** A process killed between taking the lock and committing it
///   leaves `<ref>.lock` behind, and the next updater gets [`crate::error::Error::LockHeld`]. This
///   is deliberate: a lock file's mere existence cannot distinguish a corpse from a live writer, so
///   guessing wrong means two writers corrupting a ref. The error names the file so the user can
///   remove it, exactly as Git's message does.
/// * **Dropping the guard is not silent failure.** If the lock file cannot be removed (a read-only
///   directory, a permission change mid-operation) the guard's `Drop` cannot report it, and the
///   stale lock becomes the next caller's problem. There is no better option without an explicit
///   `release`, and an explicit one would be forgotten at the early-return paths where it matters
///   most.
/// * **The lock file can be written before it is committed.** [`loose::write_locked`] is what makes
///   that useful: it lets a caller order the ref write, the reflog append and the rename so that a
///   crash leaves the ref either wholly unmoved or wholly moved, and never moved-with-no-log.
/// * **Not `Send`-safe across threads for the same reason as above**, and not `Clone`: a copied guard
///   would delete a lock file it no longer owns.
#[derive(Debug)]
pub struct RefLock {
    /// The `<ref>.lock` path that was created, kept so the guard can write to it and rename it.
    path: PathBuf,
    /// The path the lock will be renamed onto on commit.
    target: PathBuf,
    /// Cleared by [`RefLock::commit`], which is what tells `Drop` not to delete anything.
    armed: bool,
}

impl RefLock {
    /// Phase 2 — take the lock for a ref.
    ///
    /// Creates `<ref>.lock` exclusively and fails if it already exists.
    ///
    /// # Caveats
    ///
    /// * The ref itself does **not** have to exist. Locking a ref to create it is the normal case
    ///   for a first commit, and a lock implementation that required the target would make an
    ///   unborn branch uncreatable.
    /// * The parent directory is created if missing, so that locking a ref whose namespace does not
    ///   exist yet does not fail confusingly at the rename instead.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`] if the lock file already exists — "already exists" is the
    /// only way `O_EXCL` fails that this distinguishes, because that is the case the user can act
    /// on. [`crate::error::Error::IoAtPath`] for any other failure to create it.
    pub fn acquire(git_dir: &Path, name: &RefName) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — take the lock for an arbitrary path.
    ///
    /// For the files that are locked like refs but are not refs: `packed-refs.lock`, and the
    /// `MERGE_HEAD`/`MERGE_MSG` pair a merge writes.
    ///
    /// # Caveats
    ///
    /// The path is used verbatim — `.lock` is appended to it — so a caller passing a path that
    /// already ends in `.lock` gets `x.lock.lock`. That is not checked because there is no naming
    /// rule to check it against; the callers are a small, known set.
    ///
    /// # Errors
    ///
    /// As [`RefLock::acquire`].
    pub fn acquire_path(path: &Path) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — the lock file's path.
    ///
    /// # Caveats
    ///
    /// The `.lock` file, not the ref. Useful for the error message that tells a user which file to
    /// delete after a crash, and for tests that assert the lock was released.
    pub fn path(&self) -> &Path {
        todo!()
    }

    /// Phase 2 — write bytes into the lock file, then rename it over the ref.
    ///
    /// This is the whole update: the lock file is truncated and written, then `rename(2)` moves it
    /// into place. A reader of the ref sees the old inode or the new one, never a partial write.
    ///
    /// # Caveats
    ///
    /// * **The rename is the commit point.** Everything the update must do atomically relative to
    ///   other processes — appending the reflog line, in particular — has to happen before this
    ///   call, because a reader can observe the new value the instant it returns.
    /// * **The guard is disarmed by success**, so its `Drop` no longer deletes. A caller that wants
    ///   to abandon the update after writing but before renaming should drop the guard without
    ///   calling this, which removes the lock file and leaves the ref untouched — that is the
    ///   intended way to abort.
    /// * The file's mode is not adjusted. Unlike a loose object, a ref is not made read-only:
    ///   Git writes refs writable, and making them read-only would break the next update.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the lock file cannot be written or the rename fails.
    /// A failed rename leaves the lock file in place and the guard armed, so a subsequent `Drop`
    /// cleans it up — which means a caller reporting the error must not keep the guard alive
    /// indefinitely, or the user is told to delete a lock file that a still-open process owns.
    pub fn write_and_commit(&mut self, data: &[u8]) -> Result<()> {
        todo!()
    }

    /// Phase 2 — rename the lock file into place without writing it.
    ///
    /// The second half of the sequence a caller uses when it writes the lock file itself — a
    /// `packed-refs` rewrite that streams its output, or a merge writing `MERGE_HEAD` in pieces.
    ///
    /// # Caveats
    ///
    /// If nothing has been written to the lock file, this renames an **empty** file over the ref,
    /// which is a ref that exists and points nowhere. That is occasionally what a caller wants
    /// (Git does it while creating a ref before writing its value) and is never what a caller wants
    /// by accident.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the rename fails.
    pub fn commit(&mut self) -> Result<()> {
        todo!()
    }

    /// Phase 2 — release the lock without touching the ref.
    ///
    /// Removes the lock file and disarms the guard. The explicit form of what `Drop` does, for a
    /// caller that wants to report a failure to release — a lock that cannot be deleted is a lock
    /// that will block every later updater.
    ///
    /// # Caveats
    ///
    /// Releasing a lock that was never taken (a double release, or a call after
    /// [`RefLock::commit`]) is a no-op rather than an error: a committed lock is already gone, and
    /// a caller unwinding after an error should not have to know which stage it failed at.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the lock file exists and cannot be removed.
    pub fn release(&mut self) -> Result<()> {
        todo!()
    }
}

/// Phase 2 — a repository's refs, and the operations that move them.
///
/// Holds a git directory, the repository's object format, and a parsed snapshot of `packed-refs`.
/// Every read consults loose storage first and the packed snapshot second.
///
/// # Caveats
///
/// * **The packed snapshot is a snapshot.** Packs arrive and `packed-refs` is rewritten by
///   concurrent processes; call [`RefStore::refresh`] after an operation that could have moved refs
///   (a fetch, a `git gc`, a failed lookup the user insists exists). The cost of not refreshing is
///   a read that succeeds and returns a stale commit, which is worse than a failure.
/// * **No caches of resolved values.** Resolution goes to the filesystem on every call, because a
///   `HEAD` resolved once at startup and reused after a `switch` is a checkout against the wrong
///   tree. The packed snapshot is cached only because it is one file and parsing it per lookup
///   would be quadratic in the number of refs.
/// * **Cheap to construct, and does not validate the repository.** `open` reads `packed-refs` and
///   nothing else; a git directory that does not exist gives an empty-looking store rather than an
///   error. That is what lets the same code serve `init`, which operates on a directory that is
///   only half created.
/// * **The store holds no lock and no file handle.** See [`RefLock`]; an operation that updates a
///   ref takes its own lock, and two `RefStore`s over the same directory do not exclude each other
///   through anything but those locks.
/// * **Not `Clone`.** A clone would duplicate the packed snapshot, and the two copies would drift
///   the moment either refreshed. Share one with `Rc` instead.
#[derive(Debug)]
pub struct RefStore {
    /// The git directory: the one containing `HEAD`, `refs/` and `config`.
    git_dir: PathBuf,
    /// The repository's id width, from `extensions.objectFormat`. Every id read or written here is
    /// checked against it.
    format: ObjectFormat,
    /// The last parsed `packed-refs`, re-read by [`RefStore::refresh`].
    packed: PackedRefs,
}

impl RefStore {
    /// Phase 2 — open a repository's refs.
    ///
    /// Reads and parses `packed-refs`, which is the only eager work; loose refs are read on demand.
    ///
    /// # Caveats
    ///
    /// * `git_dir` is the git directory, not the work tree. Passing the work tree gives a store
    ///   whose `HEAD` does not exist, which reads as a repository with no commits rather than as an
    ///   error.
    /// * A missing `packed-refs` is not an error — it is the state of a repository that has never
    ///   run `pack-refs`, which is most of them.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptPackedRefs`] if the file exists and does not parse,
    /// [`crate::error::Error::IoAtPath`] if it exists and cannot be read, and
    /// [`crate::error::Error::InvalidRefName`] for a line naming an invalid ref.
    pub fn open(git_dir: &Path, format: ObjectFormat) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — the git directory this store reads.
    pub fn git_dir(&self) -> &Path {
        todo!()
    }

    /// Phase 2 — the repository's object format, used for every id read and written here.
    pub fn format(&self) -> ObjectFormat {
        todo!()
    }

    /// Phase 2 — the paths the git directory owns, as the `repo::Repository` glue (Batch 4) needs
    /// them.
    ///
    /// # Caveats
    ///
    /// Exposed because `Repository` builds every subsystem over the same paths and re-deriving them
    /// in each would be the place two of them disagreed about where `refs/` is.
    pub fn git_dir_paths(&self) -> GitDirPaths {
        todo!()
    }

    /// Phase 2 — re-read `packed-refs` and pick up refs that moved since the last read.
    ///
    /// # Caveats
    ///
    /// * **This replaces the snapshot wholesale.** Unlike [`crate::odb::Odb::refresh`], which only
    ///   adds, this re-reads the file — a `pack-refs` run both adds and removes entries, and a
    ///   merge-only refresh would keep refs that no longer exist.
    /// * **Loose refs are not cached, so nothing else needs refreshing.** A caller that sees a
    ///   stale value after this has seen a race it cannot fix by refreshing again.
    ///
    /// # Errors
    ///
    /// As [`RefStore::open`].
    pub fn refresh(&mut self) -> Result<()> {
        todo!()
    }

    /// Phase 2 — read a ref: loose first, then packed.
    ///
    /// The lookup that means "what does this ref say". A symbolic ref is returned unresolved.
    ///
    /// # Caveats
    ///
    /// * **A stale packed entry never wins.** If a loose file exists it is returned even when
    ///   `packed-refs` has a different value, which is the whole point of the shadowing rule.
    /// * **A ref that exists as a file but does not parse is an error, not a fall-through to
    ///   packed.** Corruption must be reported: falling back would silently return the stale packed
    ///   value and hide the broken file, and the next `pack-refs` would then delete the loose file
    ///   and cement the old value as the truth.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RefNotFound`] if neither storage has it, and the parse errors of
    /// [`loose::read`] and [`RefStore::open`].
    pub fn read(&self, name: &RefName) -> Result<RefTarget> {
        todo!()
    }

    /// Phase 2 — read a ref, treating absence as `None`.
    ///
    /// The form a compare-and-swap wants: "the old value, if any" is the setup for every safe
    /// update, including a creation that must not clobber an existing branch.
    ///
    /// # Errors
    ///
    /// Everything [`RefStore::read`] raises except [`crate::error::Error::RefNotFound`].
    pub fn try_read(&self, name: &RefName) -> Result<Option<RefTarget>> {
        todo!()
    }

    /// Phase 2 — follow a symbolic chain to the ref at its end, without resolving to an id.
    ///
    /// `HEAD` on a branch returns `refs/heads/main`; a detached `HEAD` returns `HEAD` itself.
    ///
    /// # Caveats
    ///
    /// * **The final ref may not exist.** That is the unborn-branch case on a fresh repository, and
    ///   it is a successful result here, not an error — the caller learns that `HEAD` names a branch
    ///   that has no commits yet, which is exactly what `status` needs to print.
    /// * The bound is [`MAX_SYMBOLIC_DEPTH`] hops, and a chain that exceeds it — whether or not it
    ///   would terminate — is [`crate::error::Error::SymbolicRefCycle`], because a repository Git
    ///   resolves-but-Rivet-refuses must fail loudly rather than resolve differently.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::SymbolicRefCycle`] for a cycle or an over-long chain, and the read
    /// errors of [`RefStore::read`].
    pub fn follow(&self, name: &RefName) -> Result<RefName> {
        todo!()
    }

    /// Phase 2 — resolve a ref to an object id, following symbolic refs.
    ///
    /// # Caveats
    ///
    /// * **This does not peel tags.** A ref pointing at an annotated tag resolves to the *tag*
    ///   object's id, matching `git rev-parse v1.0`. Getting the commit needs
    ///   [`RefStore::peel_to_commit`], and a caller that treats the two as the same will write a
    ///   `HEAD` pointing at a tag object, which `git status` reports as a detached head on a
    ///   non-commit.
    /// * **No object is read**, so the returned id may name an object that is not in the database —
    ///   a ref moved by a fetch that then failed, or a repository with objects deleted. Whether the
    ///   id resolves is the object database's question, and asking it here would make every `rev-parse`
    ///   touch the packfiles.
    /// * An unborn branch is [`crate::error::Error::RefNotFound`], not a zero id: no ref exists, and
    ///   a caller that wants "the current branch, whether or not it has commits" wants
    ///   [`RefStore::head_branch`], which answers without resolving.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RefNotFound`] if the chain ends at a ref that does not exist or the
    /// start does not exist; [`crate::error::Error::SymbolicRefCycle`] for a cycle; and the parse
    /// errors of [`RefStore::read`].
    pub fn resolve_oid(&self, name: &RefName) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 2 — resolve a ref to the commit it ultimately names, peeling tags.
    ///
    /// The resolution every user-facing operation actually wants: `log`, `checkout`, `merge`,
    /// `branch <name> <start>`, and `rev-parse` under `^{commit}`.
    ///
    /// # Caveats
    ///
    /// * **Peeling needs the object database**, which is why this takes one and
    ///   [`RefStore::resolve_oid`] does not. A tag chain (`tag → tag → commit`) is bounded by
    ///   `odb`'s own tag-cycle handling, not by [`MAX_SYMBOLIC_DEPTH`] — the two limits guard
    ///   different things.
    /// * **A ref that resolves to a blob or a tree is an error**, not a commit: a branch pointing at
    ///   a tree is corrupt, and Git's `rev-parse` reports it as such rather than proceeding.
    /// * A gitlink (`160000`) is not a tag and not peelable; a ref naming one ends at a commit id
    ///   that may not be in this repository at all.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::NotACommit`] if peeling ends somewhere other than a commit, plus
    /// [`crate::error::Error::TagCycle`] and [`crate::error::Error::ObjectNotFound`] from the
    /// object database, and everything [`RefStore::resolve_oid`] raises.
    pub fn peel_to_commit(&self, name: &RefName, odb: &crate::odb::Odb) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 2 — peel a resolved object id to a commit.
    ///
    /// The peeling half of [`RefStore::peel_to_commit`], for a caller that has an id from somewhere
    /// other than a ref — a `MERGE_HEAD` line, a revision the user typed, a parent id read out of a
    /// commit.
    ///
    /// # Caveats
    ///
    /// A non-tag, non-commit object is [`crate::error::Error::NotACommit`] — including a blob,
    /// which `git tag` on a blob can legitimately produce and which therefore must produce a clear
    /// error rather than a panic when someone then tries to check it out.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::NotACommit`], [`crate::error::Error::TagCycle`],
    /// [`crate::error::Error::ObjectNotFound`], and [`crate::error::Error::WrongObjectKind`] for a
    /// malformed tag whose `type` header disagrees with the object it names.
    pub fn peel_oid(&self, oid: ObjectId, odb: &crate::odb::Odb) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 2 — read one level of a tag object.
    ///
    /// The step [`RefStore::peel_oid`] loops over, exposed because `git cat-file -p v1.0` and
    /// `git show v1.0` want the tag object itself rather than what it points at.
    ///
    /// # Caveats
    ///
    /// Returns the target's id **and its kind**, because a tag records what it points at and a
    /// caller needs to know when the chain has ended at a commit rather than at another tag. The
    /// kind is the object's actual kind, which a malformed tag's `type` header may disagree with —
    /// the header is a claim, and the object is the truth.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::WrongObjectKind`] if `oid` is not a tag, and the object database's
    /// read errors.
    pub fn peel_one(&self, oid: ObjectId, odb: &crate::odb::Odb) -> Result<(ObjectId, ObjectKind)> {
        todo!()
    }

    /// Phase 2 — what `HEAD` resolves to: a branch name, or a detached commit id.
    ///
    /// # Caveats
    ///
    /// * **[`HeadState::Unborn`] is a real and common state**, not an error: a fresh repository has
    ///   `HEAD: refs/heads/main` and no `main`. Reporting it as detached or as missing makes
    ///   `status` and the first `commit` wrong in the most visible way.
    /// * **A detached `HEAD` does not need the object database here.** The id is what the file says;
    ///   whether it is a commit is a separate question answered when it is used.
    /// * A `HEAD` that is symbolic to a *tag* is legal-but-unusual; it reports as
    ///   [`HeadState::Attached`] with that tag's name, and the caller that needs a commit will fail
    ///   later at [`RefStore::peel_to_commit`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RefNotFound`] if `HEAD` itself does not exist — which means the
    /// directory is not a repository; [`crate::error::Error::SymbolicRefCycle`]; and the parse
    /// errors of [`RefStore::read`].
    pub fn head(&self) -> Result<HeadState> {
        todo!()
    }

    /// Phase 2 — the branch `HEAD` names, or `None` when it is detached or unborn.
    ///
    /// The short-form convenience under `status`, `commit` and `switch` reporting.
    ///
    /// # Caveats
    ///
    /// * **`None` is three states**, not one: detached, unborn, and a `HEAD` symbolic to something
    ///   outside `refs/heads/` (a tag). Callers that need to tell them apart must use
    ///   [`RefStore::head`] and match on [`HeadState`]; this exists for the common case where "is
    ///   there a branch and what is it called" is the whole question.
    /// * The name returned is **full** (`refs/heads/main`), not the short form. Shortening is a
    ///   display decision, and doing it here would make `branch -d` receive `main` back.
    ///
    /// # Errors
    ///
    /// As [`RefStore::head`].
    pub fn head_branch(&self) -> Result<Option<RefName>> {
        todo!()
    }

    /// Phase 2 — the commit `HEAD` names, failing on an unborn branch.
    ///
    /// The "what am I working on" lookup a `commit` uses to find its parent and a `status` uses to
    /// build the diff against.
    ///
    /// # Caveats
    ///
    /// * **An unborn branch is [`crate::error::Error::RefNotFound`]**, and a caller that must
    ///   distinguish it — `commit` on a fresh repository makes a root commit — has to use
    ///   [`RefStore::head`] instead of treating the error as fatal.
    /// * Tags are peeled, so this is the commit even when `HEAD` is symbolic to a tag.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RefNotFound`] for an unborn branch and for a missing `HEAD`, and
    /// everything [`RefStore::peel_to_commit`] raises.
    pub fn head_commit(&self, odb: &crate::odb::Odb) -> Result<ObjectId> {
        todo!()
    }

    /// Phase 2 — point `HEAD` at a branch.
    ///
    /// Writes `ref: <name>` into `HEAD` through `HEAD`'s lock. Backs `switch`, `checkout -b` and the
    /// first `commit` after `init`.
    ///
    /// # Caveats
    ///
    /// * **The branch does not have to exist.** Making `HEAD` symbolic to an unborn branch is the
    ///   normal way a branch is created and the only way `init` leaves a usable repository.
    /// * **The reflog line is the caller's**, passed as `message`; `switch` wants
    ///   `checkout: moving from <old> to <new>` and `init` wants `checkout: moving from <default>`
    ///   or nothing at all. Passing `None` writes no log line, which is what a caller that is already
    ///   logging the underlying branch move wants.
    /// * **This does not touch the work tree.** Switching the ref and updating the files are
    ///   separate operations with a safety check between them; `switch` orders them so that the
    ///   work tree is only touched after the checkout has been proven safe.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`] if `HEAD`'s lock is held, and
    /// [`crate::error::Error::IoAtPath`] if the write fails.
    pub fn set_head(&self, name: &RefName, message: Option<ReflogMessage<'_>>) -> Result<()> {
        todo!()
    }

    /// Phase 2 — point `HEAD` directly at a commit.
    ///
    /// Detaches `HEAD`: `checkout <commit>`, `checkout <tag>` and `checkout HEAD~2` all end here,
    /// and `git status` then reports `HEAD detached at <short id>` rather than a branch name.
    ///
    /// # Caveats
    ///
    /// * **The id is not checked to be a commit.** `HEAD` at a tag object is a state Git will
    ///   produce and then complain about later; refusing it here would make `checkout v1.0` fail
    ///   for a reason unrelated to what the user asked. Callers that care resolve first.
    /// * Detaching writes a reflog line to `logs/HEAD` exactly like attaching does — the reflog is
    ///   per-ref, and `HEAD` has one whatever state it is in.
    ///
    /// # Errors
    ///
    /// As [`RefStore::set_head`].
    pub fn detach_head(&self, commit: ObjectId, message: Option<ReflogMessage<'_>>) -> Result<()> {
        todo!()
    }

    /// Phase 2 — set a ref to an object id, with no expectation about its old value.
    ///
    /// # Caveats
    ///
    /// * **Unconditional, so it can lose a concurrent update.** A fetch, a push or a `branch -f`
    ///   that must not clobber a value someone else moved has to use [`RefStore::update`], which
    ///   takes an expected old value. This is the "I know what I am doing" form, and it is what
    ///   `reset --hard` and `branch -f` legitimately need.
    /// * **The lock is taken and released inside**, so a reflog append that must be atomic with the
    ///   write is not — see [`RefStore::update`] for the sequenced form.
    /// * **The ref's file is created, but the ref is not validated against the object database.**
    ///   Nothing checks that `oid` names an object, so a caller that got the id from a bad parse
    ///   writes a ref to nothing. Git does the same, and `fsck` is what finds it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`], and [`crate::error::Error::IoAtPath`] if the write fails.
    pub fn set_direct(
        &self,
        name: &RefName,
        oid: ObjectId,
        message: Option<ReflogMessage<'_>>,
    ) -> Result<()> {
        todo!()
    }

    /// Phase 2 — point a ref at another ref.
    ///
    /// Backs `symbolic-ref` and `remote set-head`, and is how a remote-tracking `HEAD` is written.
    ///
    /// # Caveats
    ///
    /// * **No cycle is prevented at write time.** Pointing `A` at `B` and then `B` at `A` succeeds
    ///   here and fails at every subsequent read, which is where the bound in
    ///   [`MAX_SYMBOLIC_DEPTH`] earns its keep. Checking would require a full resolution per write,
    ///   and Git does not check either.
    /// * The target may be unborn, like [`RefStore::set_head`]'s.
    ///
    /// # Errors
    ///
    /// As [`RefStore::set_direct`].
    pub fn set_symbolic(
        &self,
        name: &RefName,
        target: &RefName,
        message: Option<ReflogMessage<'_>>,
    ) -> Result<()> {
        todo!()
    }

    /// Phase 2 — move a ref, refusing if it is not where the caller expected it.
    ///
    /// The compare-and-swap every safe update is built on: a fetch updating a remote-tracking ref,
    /// a `pull` fast-forwarding a branch, a `push` moving a remote ref, and a `commit` extending the
    /// branch it started from.
    ///
    /// # Caveats
    ///
    /// * **`expected` is checked under the lock**, between taking it and writing. Checking before
    ///   the lock is a lost-update race, and checking after the write is no check at all — this
    ///   ordering is the entire value of the function.
    /// * **`expected: None` means "the ref must not exist"**, not "check nothing". That is the
    ///   creation case (`branch <new>`, `switch -c`) and it is the same operation: a creation is a
    ///   CAS from absent. A caller that genuinely wants to clobber uses
    ///   [`RefStore::set_direct`].
    /// * **`expected: Some(zero)` is not the same as `None`.** Passing the all-zero id means the
    ///   caller believes the ref exists and holds the null id, which no ref does; the update
    ///   therefore fails. Protocol code that wants "create or move" translates the wire's
    ///   zero-old convention to `None` itself, and must do so deliberately.
    /// * **The reflog line is appended before the ref is committed**, so a reader that sees the new
    ///   value also sees its log entry. The opposite order would leave a window where `HEAD@{0}`
    ///   disagrees with `HEAD`.
    /// * **A failed expectation is a rejected update, not corruption**, and the error carries both
    ///   ids so the caller can report "expected X, found Y" the way Git does.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`] if the lock is held;
    /// [`crate::error::Error::NonFastForward`] — reused here as the general "the ref was not where
    /// you said" failure, with `old` as the expectation and `new` as the value found — when the ref
    /// is not at `expected`; [`crate::error::Error::RefNotFound`] when `expected` is `Some` and the
    /// ref is absent, or `expected` is `None` and it exists; and
    /// [`crate::error::Error::IoAtPath`] if the write fails.
    pub fn update(
        &self,
        name: &RefName,
        new: ObjectId,
        expected: Option<ObjectId>,
        message: Option<ReflogMessage<'_>>,
    ) -> Result<()> {
        todo!()
    }

    /// Phase 2 — delete a ref from both storages.
    ///
    /// # Caveats
    ///
    /// * **Both halves must go.** Removing only the loose file leaves the packed entry as the ref's
    ///   value, so the branch reappears at its old commit — the failure that makes a deletion look
    ///   like it silently did nothing. The `packed-refs` half needs its own lock, taken after the
    ///   ref's.
    /// * **`HEAD` may not be deleted.** A repository with no `HEAD` is not a repository; the caller
    ///   must detach or repoint it first, and a refusal here is the error for that.
    /// * **The reflog is removed too**, along with any now-empty `logs/` directories. Leaving it
    ///   behind makes a recreated ref's `@{n}` resolve into its previous incarnation.
    /// * **Other refs pointing here are not checked.** Deleting a branch that another branch has
    ///   merged is the caller's business (`-d` versus `-D` is exactly that policy), and a symbolic
    ///   ref left dangling resolves as unborn, which is legal.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RefNotFound`] if the ref is in neither storage;
    /// [`crate::error::Error::SymbolicRef`] if the caller tries to delete `HEAD`;
    /// [`crate::error::Error::LockHeld`] for either lock; and
    /// [`crate::error::Error::IoAtPath`] if a file cannot be removed.
    pub fn delete(&self, name: &RefName) -> Result<()> {
        todo!()
    }

    /// Phase 2 — move a ref, taking both locks.
    ///
    /// Backs `branch -m`. Moves the loose file and the reflog, and removes the old packed entry so
    /// the old name does not survive as a shadow.
    ///
    /// # Caveats
    ///
    /// * **The old name is locked before the new one**, in that order, so two concurrent renames in
    ///   opposite directions cannot deadlock by each holding the other's source.
    /// * **`to` must not exist.** Rename is not overwrite: `branch -m a b` refuses when `b` exists,
    ///   and a rename that clobbered would destroy a branch with no reflog entry recording it.
    /// * The reflog file is **moved**, not rewritten, so `b@{1}` after a rename is `a`'s previous
    ///   position. That differs from Git, which appends a rename line to `b`'s log and leaves `a`'s
    ///   behind until expiry; either is defensible and Rivet's choice is recorded here so the
    ///   differential tests know which to expect.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RefNotFound`] if `from` is in neither storage;
    /// [`crate::error::Error::Message`] if `to` already exists;
    /// [`crate::error::Error::LockHeld`] for either lock; and
    /// [`crate::error::Error::IoAtPath`] if the move fails.
    pub fn rename(&self, from: &RefName, to: &RefName) -> Result<()> {
        todo!()
    }

    /// Phase 2 — every ref under a prefix, loose and packed, with loose values winning.
    ///
    /// The listing primitive: `branch` (prefix `refs/heads/`), `tag` (`refs/tags/`), `branch -r`
    /// (`refs/remotes/`), and `for-each-ref` (no prefix).
    ///
    /// # Caveats
    ///
    /// * **Packed entries with a loose shadow appear once**, with the loose value. A naive
    ///   concatenation lists the branch twice — once at each value — which is how a `branch` listing
    ///   ends up printing a stale commit next to the current one.
    /// * **A `packed-refs` entry whose loose file was deleted is not a ref** and must not appear.
    ///   This is the converse of shadowing and is easy to get backwards: the packed file is a
    ///   storage detail, and deleting a loose file *without* rewriting `packed-refs` is how Git
    ///   deletes a ref that is packed.
    /// * **Pseudo-refs are included when they match the prefix**, which they do only for the
    ///   empty prefix. `FETCH_HEAD` holds a `fetch`-specific format, and a caller listing everything
    ///   must either parse it as such or skip it — this returns what it reads, and a malformed
    ///   pseudo-ref makes the whole listing fail rather than being quietly dropped.
    /// * **Sorted by name** in byte order, for deterministic output.
    ///
    /// # Errors
    ///
    /// The walk errors of [`loose::list`] and the parse errors of [`RefStore::read`] for a ref that
    /// is present and broken.
    pub fn list(&self, prefix: &[u8]) -> Result<Vec<(RefName, RefTarget)>> {
        todo!()
    }

    /// Phase 2 — every local branch, with its commit id.
    ///
    /// # Caveats
    ///
    /// A branch that is unborn cannot appear — it has no id — which is why `HEAD` on a fresh
    /// repository is absent from `branch`'s output in Git too, with only the eventual first commit
    /// making it appear. A symbolic ref under `refs/heads/` (rare but legal) is resolved rather than
    /// reported as a branch whose value is another ref.
    ///
    /// # Errors
    ///
    /// As [`RefStore::list`], plus the resolution errors of [`RefStore::resolve_oid`].
    pub fn list_branches(&self) -> Result<Vec<(RefName, ObjectId)>> {
        todo!()
    }

    /// Phase 2 — every tag, with the id of what the ref names — the tag object for an annotated tag.
    ///
    /// # Caveats
    ///
    /// **Not peeled.** `v1.0` reported here is the tag object's id for an annotated tag and the
    /// commit's for a lightweight one, which is exactly `git show-ref --tags` and exactly what
    /// `rev-parse v1.0` prints. Callers that want commits peel each id themselves with
    /// [`RefStore::peel_oid`].
    ///
    /// # Errors
    ///
    /// As [`RefStore::list`] and [`RefStore::resolve_oid`].
    pub fn list_tags(&self) -> Result<Vec<(RefName, ObjectId)>> {
        todo!()
    }

    /// Phase 2 — every remote-tracking ref, with its commit id.
    ///
    /// # Caveats
    ///
    /// `refs/remotes/origin/HEAD` is a symbolic ref in most clones and resolves to the remote's
    /// default branch; it is included, matching `git branch -r`, which prints it as
    /// `origin/HEAD -> origin/main`.
    ///
    /// # Errors
    ///
    /// As [`RefStore::list`] and [`RefStore::resolve_oid`].
    pub fn list_remote(&self) -> Result<Vec<(RefName, ObjectId)>> {
        todo!()
    }

    /// Phase 2 — every ref, whatever namespace, with its resolved commit id.
    ///
    /// Backs `log --all`, `fsck`, `gc`'s reachability roots and `show-ref`.
    ///
    /// # Caveats
    ///
    /// * **Tags are peeled to commits**, so this is the set of reachability roots rather than the
    ///   set of ref values — which is what makes it *not* the same as [`RefStore::list`] with an
    ///   empty prefix, and the distinction matters the moment a caller uses the result to decide
    ///   what is reachable.
    /// * **A ref that does not peel to a commit is skipped, not fatal.** `refs/notes/…` and a tag on
    ///   a blob are both real and both unpeelable; `gc` must not refuse to run because of one.
    /// * Pseudo-refs are excluded, because `MERGE_HEAD` is an operation in progress rather than a
    ///   root of history.
    ///
    /// # Errors
    ///
    /// The walk and resolution errors of [`RefStore::list`]; a per-ref peeling failure is skipped.
    pub fn list_all(&self, odb: &crate::odb::Odb) -> Result<Vec<(RefName, ObjectId)>> {
        todo!()
    }

    /// Phase 2 — where a ref is stored, or `None` if it is nowhere.
    ///
    /// The question `pack-refs` and `gc` ask, and the one `delete` has to answer to know which
    /// halves to remove.
    ///
    /// # Caveats
    ///
    /// A loose file that fails to parse still answers [`RefLocation::Loose`] — this reports where
    /// bytes are, not whether they are good. That is deliberate: `gc` must be able to rewrite a
    /// corrupt loose ref rather than being unable to find it.
    ///
    /// # Errors
    ///
    /// None for a missing ref — `Ok(None)` — and the parse errors of [`RefStore::open`] only if the
    /// packed snapshot is consulted and is broken; it is already parsed, so in practice this is
    /// infallible.
    pub fn location(&self, name: &RefName) -> Option<RefLocation> {
        todo!()
    }

    /// Phase 2 — whether a ref exists in either storage.
    ///
    /// # Caveats
    ///
    /// `true` for a ref whose file is empty or corrupt, matching [`RefStore::location`]'s logic and
    /// Git's own distinction between "no such ref" and "broken ref".
    pub fn exists(&self, name: &RefName) -> bool {
        todo!()
    }

    /// Phase 2 — the refs currently locked by a stale lock file.
    ///
    /// Used by the error path that has just got [`crate::error::Error::LockHeld`] and wants to tell
    /// the user everything that would have to be removed, rather than one file per invocation.
    ///
    /// # Caveats
    ///
    /// A `.lock` file is evidence of a lock, not proof of a live one — see [`RefLock`]. Reporting
    /// them is all this does; it never removes one.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if `refs/` cannot be walked.
    pub fn stale_locks(&self) -> Result<Vec<PathBuf>> {
        todo!()
    }

    /// Phase 2 — the parsed `packed-refs` snapshot this store is holding.
    ///
    /// # Caveats
    ///
    /// A snapshot, as the type-level caveat says. Exposed for `pack-refs` and `gc`, which need to
    /// read-modify-write the whole file and would otherwise re-parse it; a caller reading a value
    /// must go through [`RefStore::read`] so that loose shadowing applies.
    pub fn packed(&self) -> &PackedRefs {
        todo!()
    }

    /// Phase 2 — write `packed-refs` from the given set, replacing the file and the snapshot.
    ///
    /// The commit half of `pack-refs` and `gc`.
    ///
    /// # Caveats
    ///
    /// * **The whole file is rewritten**, under `packed-refs.lock`. There is no append form: an
    ///   entry's peel line follows it and the file is sorted, so a single insertion still means
    ///   rewriting.
    /// * **The traits written must be honest.** See [`PackedRefs::set_traits`]; the caller building
    ///   the set is the only one that knows whether it peeled every tag.
    /// * **The store's snapshot is updated on success only.** A failed write leaves the in-memory
    ///   value matching the file on disk, which is the only state a caller can reason about.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockHeld`], and [`crate::error::Error::IoAtPath`] if the write fails.
    pub fn write_packed(&mut self, packed: PackedRefs) -> Result<()> {
        todo!()
    }

    /// Phase 2 — the reflog for a ref, oldest entry first.
    ///
    /// # Caveats
    ///
    /// A ref with no log is an empty vector, not an error; see [`reflog::read`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptReflog`] and [`crate::error::Error::IoAtPath`].
    pub fn reflog(&self, name: &RefName) -> Result<Vec<ReflogEntry>> {
        todo!()
    }

    /// Phase 2 — append to a ref's reflog without moving the ref.
    ///
    /// The rarely-needed direction: a `reset` that records an `ORIG_HEAD` movement, or a `gc`
    /// logging a ref it repacked. Normal updates log through their own
    /// [`RefStore::update`] call so that the append happens under the ref's lock.
    ///
    /// # Caveats
    ///
    /// **This takes no lock**, because it does not touch the ref — but it also means a concurrent
    /// ref update can interleave, and the log will record two updates in an order that may not
    /// match what happened. Callers that are updating the ref themselves must not use this.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the log cannot be written.
    pub fn append_reflog(&self, name: &RefName, entry: &ReflogEntry) -> Result<()> {
        todo!()
    }

    /// Phase 2 — decide which namespace an abbreviated name refers to.
    ///
    /// Git's disambiguation order for `rev-parse main`: the name as written, then `refs/<name>`,
    /// then `refs/tags/<name>`, then `refs/heads/<name>`, then `refs/remotes/<name>`, then
    /// `refs/remotes/<name>/HEAD`. The first that exists wins, which is why a tag and a branch with
    /// the same short name resolve to the tag.
    ///
    /// # Caveats
    ///
    /// * **Ambiguity is a warning, not an error, in Git** — it resolves to the tag and prints
    ///   "refname 'x' is ambiguous" on stderr. Returning the choice without the warning is correct
    ///   for resolution; a caller that wants the diagnostic has to detect the ambiguity itself, so
    ///   the implementation checks each candidate with [`RefStore::exists`] rather than hiding
    ///   which ones were present.
    /// * **The name as written is tried first**, so a fully-qualified `refs/heads/main` resolves
    ///   even when something else shadows the short form. The ordering is what Git specifies and it
    ///   costs nothing to follow.
    /// * **A candidate that exists but is broken does not stop the search** in Git, which warns and
    ///   continues; a caller that needs strictness calls [`RefStore::read`] on the result.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::RefNotFound`] if no candidate exists, and
    /// [`crate::error::Error::InvalidRefName`] if `short` cannot be spelled as a ref name in any of
    /// the candidate namespaces.
    pub fn resolve_short(&self, short: &bstr::BStr) -> Result<RefName> {
        todo!()
    }
}

/// Phase 2 — what `HEAD` currently is.
///
/// # Caveats
///
/// * **The three states are genuinely distinct and the CLI must not collapse them.** Attached
///   prints `On branch main`; detached prints `HEAD detached at <id>`; unborn prints
///   `On branch main\n\nNo commits yet`. A `bool is_detached` plus an `Option<RefName>` loses the
///   unborn case, which is the state every brand-new repository is in and the first thing a user
///   sees.
/// * **`Detached` does not imply the id is a commit.** It is what `HEAD`'s file says; a `HEAD` at a
///   tag object is represented here and rejected later, at peeling time.
/// * An unborn branch is a branch, so [`HeadState::Unborn`] carries a [`RefName`] — the branch that
///   does not exist yet. That name is what the first commit will create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadState {
    /// `HEAD` is a symbolic ref to a branch that exists and resolves.
    Attached {
        /// The branch's full name, e.g. `refs/heads/main`.
        branch: RefName,
        /// What the branch points at, which for an annotated tag on the branch is the tag object.
        oid: ObjectId,
    },
    /// `HEAD` is a symbolic ref to a branch with no commits yet.
    Unborn {
        /// The branch that will be created by the first commit.
        branch: RefName,
    },
    /// `HEAD` holds an object id directly.
    Detached {
        /// The id `HEAD` names.
        oid: ObjectId,
    },
}

impl HeadState {
    /// Phase 2 — the branch `HEAD` is on, or `None` when detached.
    ///
    /// # Caveats
    ///
    /// Returns the branch for [`HeadState::Unborn`] as well as [`HeadState::Attached`], because
    /// "which branch am I on" has an answer in both — a fresh repository is on `main` before `main`
    /// exists. A caller that needs the branch to *resolve* must check for `Attached` specifically.
    pub fn branch(&self) -> Option<&RefName> {
        todo!()
    }

    /// Phase 2 — the id `HEAD` names, if there is one.
    ///
    /// # Caveats
    ///
    /// `None` for an unborn branch and for a symbolic `HEAD` that does not resolve — the two cases
    /// where the user has asked "where am I" and the honest answer is "nowhere yet".
    pub fn oid(&self) -> Option<ObjectId> {
        todo!()
    }

    /// Phase 2 — whether `HEAD` is detached.
    ///
    /// # Caveats
    ///
    /// `false` for an unborn branch. That is the right answer for the question this asks, but it is
    /// not "the branch exists", and code that commits or checks out must not read it as such.
    pub const fn is_detached(&self) -> bool {
        todo!()
    }

    /// Phase 2 — whether `HEAD` is on a branch that has no commits yet.
    pub const fn is_unborn(&self) -> bool {
        todo!()
    }
}

/// Phase 2 — the paths inside a git directory that the refs and the rest of the repository share.
///
/// # Caveats
///
/// A plain value with no I/O, returned by [`RefStore::git_dir_paths`] so that `Repository` and its
/// subsystems agree on one spelling of `refs/`, `logs/` and `packed-refs` rather than each
/// deriving them. The layout for a **linked worktree** differs — refs live in the common directory
/// and only `HEAD` and `logs/HEAD` are per-worktree — which is out of scope for v1 (plan §1) and is
/// why this is one derived struct rather than a path built at each call site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDirPaths {
    /// The git directory itself.
    pub git_dir: PathBuf,
    /// `refs/`, the root of loose ref storage.
    pub refs_dir: PathBuf,
    /// `logs/`, the root of reflog storage.
    pub logs_dir: PathBuf,
    /// `packed-refs`, the single file holding every ref that has been packed.
    pub packed_refs: PathBuf,
    /// `HEAD`, which is a ref and also the only one with no namespace.
    pub head: PathBuf,
}

impl GitDirPaths {
    /// Phase 2 — derive every path from a git directory.
    ///
    /// # Caveats
    ///
    /// Nothing is checked to exist, so this works on a directory `init` is halfway through
    /// creating. That is the point: `init` needs the paths before it has files, and a constructor
    /// that validated would be unusable in the one place it is most needed.
    pub fn new(git_dir: &Path) -> Self {
        todo!()
    }
}

/// Phase 2 — check a ref name's validity without building a [`RefName`].
///
/// The predicate under [`RefName::parse`], exposed for the CLI's `check-ref-format` and for the
/// revision parser, which sees user input that is *probably* a ref name and needs to know whether
/// it could be one before committing to that interpretation.
///
/// # Caveats
///
/// A `bool` rather than a `Result`, because the caller here is deciding which of several
/// interpretations to try, and the specific rule that failed is only interesting once the answer is
/// "this was meant to be a ref". Code that wants the reason uses [`RefName::parse`].
pub fn is_valid_ref_name(name: &[u8]) -> bool {
    todo!()
}
