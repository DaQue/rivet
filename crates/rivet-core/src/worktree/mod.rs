//! Phase 4 — the work tree: the files a repository is checked out into, and what differs in them.
//!
//! The work tree is the only part of a repository that is not under Rivet's control. Refs, the
//! index and the object database are files Git's own formats describe, and every writer of them
//! agrees on what a correct one looks like; a work tree is a directory the user edits with other
//! programs, on a filesystem that may be case-insensitive, may not store an executable bit, and
//! may be missing files the index says are there.
//!
//! # What this module owns
//!
//! * **The walk** ([`Worktree::walk`]) — a recursive listing of the work tree, with `.git`
//!   skipped, ignore rules applied, and each entry classified as a file, a directory or
//!   something else. Everything that asks "what is on disk" comes through here.
//! * **Status** ([`Worktree::status`], [`status::compute`]) — the three-way comparison of
//!   `HEAD`'s tree, the index and the work tree that produces the staged/unstaged/untracked
//!   lists `rivet status` prints.
//! * **Checkout** ([`CheckoutPlan`]) — bringing the work tree to a tree's contents, with the
//!   safety check of plan §5 in front of every write.
//! * **Add and rm** ([`add`]) — turning work tree files into index entries, and the reverse.
//! * **The stat cache** ([`StatCache`], [`StatData`]) — the `lstat` results the index stores,
//!   and the comparison that decides whether a file is worth reading.
//!
//! # The rule that this module exists to enforce
//!
//! **Nothing touches the work tree before the whole operation has been proven safe.** Plan §5
//! and the tinyvcs `victim.txt` bug are the same failure: a checkout computed the paths that
//! differ between the current and target trees, then wrote them, and a file that had been
//! modified in the work tree and deleted in the target was silently destroyed. The rule that
//! prevents it is stated once, in [`CheckoutPlan::validate`]:
//!
//! > For every path the operation would touch, refuse if the work tree file differs from the
//! > index *or* the index differs from `HEAD`.
//!
//! The second clause is the one that catches the `victim.txt` case, and it is the one a
//! from-scratch implementation leaves out, because "the file is dirty" is the intuitive
//! question and "the file is clean here but the index has diverged from HEAD" is not.
//!
//! # Caveats that bite
//!
//! * **Paths are bytes.** A filename that is not valid UTF-8 is legal on Linux and the index
//!   stores it exactly. Every path here is [`BString`], and a conversion to `String` is always
//!   lossy — two different paths can convert to the same one, so a comparison must never go
//!   through `String`.
//! * **`/` is the only separator, on every platform.** The index's paths use `/` even on
//!   Windows, so joining a path to the work tree root means splitting on `/` and pushing
//!   components, not `Path::join` on a whole string.
//! * **Every path is relative to the work tree root and cannot escape it.** A path that is
//!   absolute, or that contains a `..` component, or that normalises outside the root, is
//!   [`crate::error::Error::InvalidPath`] — and that check belongs before any filesystem call,
//!   because on Windows a component containing `:` is a drive letter and a component named
//!   `CON` is a device.
//! * **`lstat`, never `stat`.** A symlink's content is its target, stored as a blob in
//!   [`EntryMode::Link`]; following it makes the entry look like whatever it points at, and a
//!   checkout that followed one would write through it. Symlinks are never followed, for
//!   reading or for writing.
//! * **`core.filemode` and `core.symlinks` change what the filesystem means.** On a filesystem
//!   that does not store the executable bit, or with `core.filemode = false`, every regular file
//!   classifies as [`EntryMode::Blob`]; with `core.symlinks = false` a symlink was checked out
//!   as a regular file holding the target's text and must classify as one. Reading these once,
//!   at construction, is what keeps `status` from reporting spurious type changes on Windows.
//! * **A bare repository has no work tree.** [`Worktree::root`] is then `None` and every
//!   operation that needs files returns [`crate::error::Error::BareRepository`] rather than
//!   guessing a directory.
//! * **The work tree is not locked.** Git's `.lock` files cover refs, the index and the config;
//!   nothing covers the files themselves, because users edit them. Two Rivet processes
//!   checking out different commits into one work tree will interleave, and the index's own
//!   lock is the only thing that keeps the *index* consistent.

use std::path::{Path, PathBuf};

use bstr::{BStr, BString};

use crate::config::Config;
use crate::error::Result;
use crate::hash::ObjectFormat;
use crate::ignore::{IgnoreStack, PathKind};
use crate::index::Index;
use crate::object::EntryMode;
use crate::odb::Odb;

pub mod add;
pub mod checkout;
pub mod stat;
pub mod status;

pub use add::{AddOptions, AddOutcome, RmOptions, add_paths, build_index_entry, rm_paths};
pub use checkout::{
    CheckoutAction, CheckoutEntry, CheckoutOptions, CheckoutPlan, paths_collide, remove_path,
    write_blob,
};
pub use stat::{StatCache, StatData, index_mtime};
pub use status::{
    ChangeKind, Status, StatusEntry, StatusOptions, UntrackedMode, compute, staged_changes,
    unstaged_changes, untracked_paths,
};

/// Phase 4 — the work tree of one repository, and the settings that decide how it is read.
///
/// Holds the root path and the three `core.*` knobs that change what a `lstat` means. It has no
/// object database and no index: every operation takes the ones it needs, because a
/// [`Worktree`] is cheap to build and a caller that had to build a whole [`crate::repo::Repository`]
/// to compare one file would pay for packs and refs it never reads.
///
/// # Caveats
///
/// * **Holds no file handle and caches nothing.** Two [`Status`] calls see two different
///   filesystems, which is correct: the work tree is the part of the repository that changes
///   under Rivet's feet, and a cached listing is a listing that silently omits a file the user
///   just created.
/// * **`root` is `None` for a bare repository**, and every path-taking method then fails with
///   [`crate::error::Error::BareRepository`]. That is preferable to a constructor that cannot
///   build one, because `init` and `open` must describe both kinds of repository with one type.
/// * **`ignore_case` is `core.ignorecase`, which describes the *filesystem*, not the
///   repository.** Git sets it at `init` from a probe and never re-checks; a repository copied
///   between a case-sensitive and a case-insensitive filesystem can have the wrong value, and
///   the only symptom is a checkout that reports a collision it should not, or fails to report
///   one it should. Rivet reads it and does not probe.
/// * **Constructing one performs no validation.** A root that does not exist, or exists and is
///   a file, gives a [`Worktree`] whose every operation fails at the filesystem. Validating
///   would make `init` — which builds one over a directory it is about to create — impossible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// The work tree root, or `None` when the repository is bare.
    root: Option<PathBuf>,
    /// The repository's id width, needed to hash a work tree file into an index entry.
    format: ObjectFormat,
    /// `core.filemode` — whether the filesystem's executable bit is meaningful.
    filemode: bool,
    /// `core.symlinks` — whether a symlink can be stored as one, or was checked out as a file.
    symlinks: bool,
    /// `core.ignorecase` — whether the filesystem folds case, and therefore whether two index
    /// entries can collide on disk.
    ignore_case: bool,
    /// `core.protectNTFS` — whether Windows-only path hazards are refused. Off by default on
    /// Unix, on by default on Windows, exactly as Git defaults it.
    protect_ntfs: bool,
}

impl Worktree {
    /// Phase 4 — build a work tree handle over `root`, reading the `core.*` knobs from `config`.
    ///
    /// # Caveats
    ///
    /// * **Nothing is checked to exist.** See the type caveat; `init` needs a handle over a
    ///   directory that is not there yet.
    /// * **`config` is the composed config, not the file's.** The knobs live in the repository's
    ///   local config but a global `core.filemode` is legal, and the caller composing the levels
    ///   (`repo::Repository`) is the only one that knows the precedence.
    /// * **`root: None` builds the bare form**, which is what `open` passes for a bare
    ///   repository rather than inventing a directory.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::ConfigInvalidValue`] for a `core.filemode`, `core.symlinks`,
    /// `core.ignorecase` or `core.protectNTFS` that is present and not a boolean.
    pub fn new(root: Option<&Path>, format: ObjectFormat, config: &Config) -> Result<Self> {
        todo!()
    }

    /// Phase 4 — the work tree root, or `None` when the repository is bare.
    ///
    /// # Caveats
    ///
    /// `None` is a real answer and not an error; a caller that needs a directory uses
    /// [`Worktree::require_root`], which is the same query with the bare-repository case turned
    /// into [`crate::error::Error::BareRepository`].
    pub fn root(&self) -> Option<&Path> {
        todo!()
    }

    /// Phase 4 — the work tree root, failing on a bare repository.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::BareRepository`] with `op` naming the operation that needed a work
    /// tree, because the user's next question is always "which command was that".
    pub fn require_root(&self, op: &'static str) -> Result<&Path> {
        todo!()
    }

    /// Phase 4 — whether the repository has no work tree.
    pub fn is_bare(&self) -> bool {
        self.root().is_none()
    }

    /// Phase 4 — the repository's object format.
    pub fn format(&self) -> ObjectFormat {
        todo!()
    }

    /// Phase 4 — whether the executable bit is meaningful on this filesystem.
    pub fn filemode(&self) -> bool {
        todo!()
    }

    /// Phase 4 — whether symlinks can be stored as symlinks here.
    pub fn symlinks(&self) -> bool {
        todo!()
    }

    /// Phase 4 — whether the filesystem folds case.
    pub fn ignore_case(&self) -> bool {
        self.root.as_ref().map(|p| p.join("case_test").exists()).unwrap_or(false)
    }

    /// Phase 4 — turn a work-tree-relative path into a filesystem path.
    ///
    /// # Caveats
    ///
    /// * **The path is validated, not merely joined.** Absolute paths, `..` components, empty
    ///   components and NUL bytes are refused before any filesystem call; with `core.protectNTFS`
    ///   on, a component containing `:`, `\`, a trailing dot or space, or a reserved device name
    ///   is refused too. This is the traversal guard, and it is why a caller must not build the
    ///   path with `Path::join` itself.
    /// * **The result is not canonicalised.** Canonicalising would follow symlinks, which turns a
    ///   check on the path into a check on whatever the path currently points at — and the point
    ///   of the check is that the path cannot be made to point elsewhere.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidPath`] for a path that violates the rules above, and
    /// [`crate::error::Error::BareRepository`] when there is no root to join to.
    pub fn join(&self, path: &BStr) -> Result<PathBuf> {
        todo!()
    }

    /// Phase 4 — the work-tree-relative path for a filesystem path under the root.
    ///
    /// The inverse of [`Worktree::join`], used when a walk finds an entry and needs the name the
    /// index would store.
    ///
    /// # Caveats
    ///
    /// Components are joined with `/` whatever the platform's separator is, because the index
    /// and every tree store `/`. A caller that rendered the path with `Path::display` gets `\`
    /// on Windows, which matches no index entry.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::OutsideWorktree`] if `path` is not under the root. A path outside
    /// the work tree has no index-relative spelling and must not be guessed at by stripping
    /// components until something fits.
    pub fn relative_path(&self, path: &Path) -> Result<BString> {
        todo!()
    }

    /// Phase 4 — walk the work tree, applying ignore rules.
    ///
    /// # Caveats
    ///
    /// * **`.git` is skipped**, and skipped by name at every level rather than by remembering
    ///   where the git directory is: a submodule's `.git` is a file and a linked worktree's is
    ///   elsewhere, and neither should be listed as content.
    /// * **An ignored directory is not descended into.** That is both the performance win and
    ///   the reason a `!build/keep` cannot re-include anything — see
    ///   [`crate::ignore::IgnoreStack::should_descend`].
    /// * **Symlinks are listed, never followed.** A symlink to a directory is one entry of kind
    ///   [`PathKind::File`], because Git stores it as a blob and following it would walk
    ///   whatever it points at, including out of the work tree.
    /// * **The order is not the filesystem's.** Entries come back sorted by path with bytes
    ///   compared directly, so that a walk is reproducible across machines — a directory
    ///   listing's order is whatever the filesystem feels like and differs between ext4 and APFS.
    /// * **`include_ignored` costs a second pass' worth of work** and is only what
    ///   `status --ignored` asks for; a plain status must not set it, because collecting the
    ///   ignored set means descending into directories the walk would otherwise skip entirely.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if a directory cannot be read — a permission error
    /// mid-walk is reported rather than silently truncating the listing, because a truncated
    /// listing makes every file under it look deleted.
    pub fn walk(
        &self,
        ignore: &IgnoreStack,
        options: &WorktreeWalkOptions,
    ) -> Result<Vec<WorktreeEntry>> {
        todo!()
    }

    /// Phase 4 — walk one directory, without recursing.
    ///
    /// The step [`Worktree::walk`] loops over, exposed because `add <dir>` and the untracked
    /// scan want one level and would otherwise filter a full walk.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the directory cannot be read, and
    /// [`crate::error::Error::InvalidPath`] for `dir` itself.
    pub fn walk_dir(&self, dir: &BStr, ignore: &IgnoreStack) -> Result<Vec<WorktreeEntry>> {
        todo!()
    }

    /// Phase 4 — `lstat` one path, as the index's stat cache would record it.
    ///
    /// # Caveats
    ///
    /// * **`Ok(None)` for a path that is not there**, which is the answer `status` needs for a
    ///   deleted file and is not an error. An `io::ErrorKind::NotFound` is the only failure
    ///   folded in this way; a permission error is reported.
    /// * **The mode is classified through `core.filemode` and `core.symlinks`**, so the answer
    ///   is the mode Git would store, not the mode the filesystem reports.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] for a path that exists and cannot be stat'd, and
    /// [`crate::error::Error::InvalidPath`] for one that escapes the root.
    pub fn stat(&self, path: &BStr) -> Result<Option<StatData>> {
        todo!()
    }

    /// Phase 4 — read a file's content as Git would store it.
    ///
    /// For a regular file this is its bytes; for a symlink it is the **link target**, which is
    /// what Git stores in place of the file — and reading it with `read` rather than `readlink`
    /// follows the link and returns the wrong bytes.
    ///
    /// # Caveats
    ///
    /// * **The whole file is read into memory**, because Git's object model has no streaming
    ///   form: an object's length is in its header and its id is over all of it. A multi-gigabyte
    ///   file is a real cost and there is no signature change that avoids it.
    /// * **With `core.symlinks = false`** the file on disk is the target's text, and it is read
    ///   as an ordinary file even though the index entry says [`EntryMode::Link`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::IoAtPath`] if the file cannot be read or the link target cannot be
    /// resolved, and [`crate::error::Error::InvalidPath`] for a path that escapes the root.
    pub fn read_file(&self, path: &BStr) -> Result<Vec<u8>> {
        todo!()
    }

    /// Phase 4 — hash a file's content into an object id, without storing it.
    ///
    /// The primitive under `status`, `add` and `diff`: comparing two blobs means knowing this
    /// file's id, and the id must be computed the same way for a work tree file and for a blob
    /// in the object database or the two never compare equal.
    ///
    /// # Caveats
    ///
    /// **The mode is not part of the hash.** A `100644` file and its `100755` twin that hold the
    /// same bytes have the same id, which is why a mode change is a diff with no content
    /// difference and why `status` has to compare modes separately.
    ///
    /// # Errors
    ///
    /// As [`Worktree::read_file`], plus the hasher's collision-detection path.
    pub fn hash_file(&self, path: &BStr) -> Result<crate::hash::ObjectId> {
        let buf = self.read_file(path)?;
        let format = ObjectFormat::Sha1;
        let mut hasher = format.hasher();
        hasher.update(&buf);
        hasher.finish()
    }

    /// Phase 4 — compute the status of the work tree.
    ///
    /// The three-way comparison of `HEAD`'s tree, the index and the work tree. `head_tree` is
    /// the tree of the commit `HEAD` names, or `None` on an unborn branch, where every staged
    /// path is an addition.
    ///
    /// # Caveats
    ///
    /// * **This does not read `HEAD`.** The caller supplies the tree, because a [`Worktree`] has
    ///   no refs — and because the caller may want the status against something else, which is
    ///   what `status` after a `reset --soft` needs.
    /// * **The index's own mtime is read here.** It is the input to the racy-git rule, and
    ///   taking it from the filesystem rather than from a cached value is what makes an entry
    ///   written in the same clock tick as its file be re-hashed instead of trusted.
    /// * **A tracked file is never ignored** in the untracked list's sense: it is in the index,
    ///   so it is reported as modified, deleted or type-changed whatever the ignore rules say.
    ///   Ignore rules only decide whether an *untracked* file is listed or filtered.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::BareRepository`] if there is no work tree,
    /// [`crate::error::Error::CorruptIndex`]-family errors from the index's stat cache only in
    /// the sense of a malformed entry, and the object database's read errors for the blobs
    /// [`Status`] compares against.
    pub fn status(
        &self,
        index: &Index,
        head_tree: Option<crate::hash::ObjectId>,
        odb: &Odb,
        ignore: &IgnoreStack,
        options: &StatusOptions,
    ) -> Result<Status> {
        todo!()
    }
}

/// Phase 4 — one entry found by a work tree walk.
///
/// # Caveats
///
/// * **`kind` and `mode` answer different questions and both are needed.** [`PathKind`] is what
///   the filesystem reports — the input an ignore rule needs to decide whether a trailing `/`
///   applies — while [`EntryMode`] is what Git would store, which differs for a file whose
///   executable bit is present but ignored by `core.filemode`.
/// * **A directory is not an index entry.** Git does not track them and the walk reports them
///   only so that a caller can recurse and so that ignore rules can match on them; a caller
///   building index entries must skip [`EntryMode::Tree`].
/// * **`size` is the length of what would be stored**, so it is the link target's length for a
///   symlink and the file's own length otherwise. It is not a hash and it is not authoritative
///   after the walk returns — a file can change between the `lstat` and the read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// The path, relative to the work tree root, `/`-separated.
    pub path: BString,
    /// What the filesystem says this is.
    pub kind: PathKind,
    /// What Git would store this as.
    pub mode: EntryMode,
    /// The stored content's length in bytes, from the `lstat`.
    pub size: u64,
    /// Whether an ignore rule matched it. Meaningful only when the walk was asked to collect
    /// ignored entries.
    pub ignored: bool,
}

/// Phase 4 — how much a work tree walk should collect.
///
/// # Caveats
///
/// The defaults matter: a plain `status` wants untracked files but not ignored ones, and
/// collecting ignored entries forces the walk into directories it would otherwise skip, so
/// `include_ignored` is off unless a caller asked for it. There is no `Default` impl — a caller
/// builds one from [`WorktreeWalkOptions::new`], which is deliberate, because a struct-literal
/// default is how "we collect ignored files everywhere" gets in by accident.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeWalkOptions {
    /// Whether to report files matched by ignore rules.
    pub include_ignored: bool,
    /// Whether to descend into directories at all. A caller that only wants the top level sets
    /// this false.
    pub recursive: bool,
    /// A work-tree-relative directory to start from, empty for the root.
    pub prefix: BString,
}

impl WorktreeWalkOptions {
    /// Phase 4 — the options `status` uses: recursive, untracked files included, ignored ones
    /// not.
    pub fn new() -> Self {
        todo!()
    }

    /// Phase 4 — set whether ignored files are collected.
    pub fn include_ignored(&mut self, on: bool) -> &mut Self {
        todo!()
    }

    /// Phase 4 — restrict the walk to a subtree.
    pub fn prefix(&mut self, prefix: &BStr) -> &mut Self {
        todo!()
    }
}

/// Phase 4 — validate a work-tree-relative path.
///
/// The check [`Worktree::join`] and [`Worktree::stat`] perform, exposed so that a caller
/// validating a list of pathspecs can do it before it has a [`Worktree`] in hand — which is
/// where a bad pathspec should be reported, not after half the list has been applied.
///
/// # Caveats
///
/// * **An empty path is legal** and means the root, which is what `add -A` and a walk's start
///   pass.
/// * **A path with a trailing `/` is legal** and means a directory; the separator is dropped by
///   the caller that joins components, not here, because the difference between `build` and
///   `build/` is meaningful to a pathspec.
/// * **The `protect_ntfs` argument is not derivable from the path.** It comes from config and
///   from the platform, and passing it explicitly is what lets the differential tests run the
///   Windows rules on Linux.
///
/// # Errors
///
/// [`crate::error::Error::InvalidPath`] for an absolute path, a `..` component, a NUL, or — with
/// `protect_ntfs` — a component containing `\` or `:`, ending in a dot or a space, or naming a
/// Windows device.
pub fn check_relative_path(path: &BStr, protect_ntfs: bool) -> Result<()> {
    todo!()
}
