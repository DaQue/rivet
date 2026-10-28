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

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use bstr::{BStr, BString};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::hash::ObjectFormat;
use crate::ignore::{IgnoreStack, PathKind};
use crate::index::Index;
use crate::object::{EntryMode, ObjectKind, hash_object};
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
    /// `core.autocrlf` / `core.eol` / `core.safecrlf` / attribute settings for the clean and
    /// smudge conversions.
    convert: crate::convert::ConvertConfig,
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
        let root = root.map(PathBuf::from);
        let filemode = config
            .get_bool(BStr::new(b"core.filemode"))?
            .unwrap_or(true);
        let symlinks = config
            .get_bool(BStr::new(b"core.symlinks"))?
            .unwrap_or(true);
        let ignore_case = config
            .get_bool(BStr::new(b"core.ignorecase"))?
            .unwrap_or(false);
        let protect_ntfs = config
            .get_bool(BStr::new(b"core.protectNTFS"))?
            .unwrap_or(cfg!(windows));
        let convert = crate::convert::ConvertConfig::from_config(config)?;
        Ok(Self {
            root,
            format,
            filemode,
            symlinks,
            ignore_case,
            protect_ntfs,
            convert,
        })
    }

    /// The common git directory, found from `<root>/.git` (a directory, or a `gitdir:` file for a
    /// linked work tree), where `info/attributes` lives.
    fn common_git_dir(&self) -> Option<PathBuf> {
        let root = self.root()?;
        let dot_git = root.join(".git");
        let mut dir = if dot_git.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = Path::new(text.strip_prefix("gitdir:")?.trim());
            if target.is_absolute() {
                target.to_path_buf()
            } else {
                root.join(target)
            }
        } else {
            dot_git
        };
        if let Ok(common) = std::fs::read_to_string(dir.join("commondir")) {
            dir = dir.join(common.trim());
        }
        Some(dir)
    }

    /// Phase 4 — the attribute stack that applies to `path`.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::BareRepository`] when there is no work tree.
    pub fn attr_stack(&self, path: &BStr) -> Result<crate::convert::AttrStack> {
        let root = self.require_root("attributes")?;
        Ok(crate::convert::AttrStack::load(
            root,
            self.common_git_dir().as_deref(),
            &self.convert,
            path,
        ))
    }

    /// Phase 4 — the clean conversion (work tree → blob) of the content of a regular file.
    ///
    /// `index_has_cr` says whether the index blob for the path holds a CR (asked only when it
    /// matters); `report` enables the `core.safecrlf` warning, which git only gives when it
    /// stores the object.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Message`] for an attribute that cannot be honoured, and for
    /// `core.safecrlf=true` when the conversion would not round-trip.
    pub fn clean_content(
        &self,
        path: &BStr,
        data: Vec<u8>,
        index_has_cr: &mut dyn FnMut() -> bool,
        report: bool,
    ) -> Result<Vec<u8>> {
        let stack = self.attr_stack(path)?;
        self.convert.clean(&stack, path, data, index_has_cr, report)
    }

    /// Phase 4 — the smudge conversion (blob → work tree) of a regular file's content.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Message`] for an attribute that cannot be honoured.
    pub fn smudge_content(&self, path: &BStr, data: Vec<u8>) -> Result<Vec<u8>> {
        let stack = self.attr_stack(path)?;
        self.convert.smudge(&stack, path, data)
    }

    /// Phase 4 — the work tree root, or `None` when the repository is bare.
    ///
    /// # Caveats
    ///
    /// `None` is a real answer and not an error; a caller that needs a directory uses
    /// [`Worktree::require_root`], which is the same query with the bare-repository case turned
    /// into [`crate::error::Error::BareRepository`].
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Phase 4 — the work tree root, failing on a bare repository.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::BareRepository`] with `op` naming the operation that needed a work
    /// tree, because the user's next question is always "which command was that".
    pub fn require_root(&self, op: &'static str) -> Result<&Path> {
        match self.root.as_deref() {
            Some(root) => Ok(root),
            None => Err(Error::BareRepository {
                path: BString::default(),
                op,
            }),
        }
    }

    /// Phase 4 — whether the repository has no work tree.
    pub fn is_bare(&self) -> bool {
        self.root().is_none()
    }

    /// Phase 4 — the repository's object format.
    pub fn format(&self) -> ObjectFormat {
        self.format
    }

    /// Phase 4 — whether the executable bit is meaningful on this filesystem.
    pub fn filemode(&self) -> bool {
        self.filemode
    }

    /// Phase 4 — whether symlinks can be stored as symlinks here.
    pub fn symlinks(&self) -> bool {
        self.symlinks
    }

    /// Phase 4 — whether the filesystem folds case.
    pub fn ignore_case(&self) -> bool {
        self.ignore_case
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
        let root = self.require_root("path lookup")?;
        check_relative_path(path, self.protect_ntfs)?;
        let mut out = root.to_path_buf();
        for comp in path.split(|b| *b == b'/') {
            if comp.is_empty() {
                continue;
            }
            out.push(OsStr::from_bytes(comp));
        }
        Ok(out)
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
        let root = self.require_root("relative path")?;
        let rel = path
            .strip_prefix(root)
            .map_err(|_| Error::OutsideWorktree(BString::from(path.as_os_str().as_bytes())))?;
        let mut out: Vec<u8> = Vec::new();
        for comp in rel.components() {
            if !out.is_empty() {
                out.push(b'/');
            }
            out.extend_from_slice(comp.as_os_str().as_bytes());
        }
        Ok(BString::from(out))
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
        let mut out = Vec::new();
        // The stack is cloned so the walk can push each directory's `.gitignore` as it descends
        // and truncate as it leaves; a caller's stack is left exactly as it was found.
        let mut stack = ignore.clone();
        self.walk_into(options.prefix.as_ref(), &mut stack, options, &mut out)?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// One directory of [`Worktree::walk`], with the ignore stack pushed and truncated around it.
    fn walk_into(
        &self,
        dir: &BStr,
        ignore: &mut IgnoreStack,
        options: &WorktreeWalkOptions,
        out: &mut Vec<WorktreeEntry>,
    ) -> Result<()> {
        let depth = dir.split(|b| *b == b'/').filter(|c| !c.is_empty()).count();
        ignore.push_dir(dir)?;
        let entries = self.walk_dir(dir, ignore)?;
        for entry in entries {
            let ignored = entry.ignored;
            let is_dir = entry.kind == PathKind::Directory;
            if ignored && !options.include_ignored {
                continue;
            }
            if is_dir {
                let descend =
                    !ignored && options.recursive && ignore.should_descend(entry.path.as_ref())?;
                let path = entry.path.clone();
                out.push(entry);
                if descend {
                    self.walk_into(BStr::new(&path), ignore, options, out)?;
                }
            } else {
                out.push(entry);
            }
        }
        ignore.truncate(depth);
        Ok(())
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
        let fs_dir = self.join(dir)?;
        let rd = std::fs::read_dir(&fs_dir).map_err(|e| Error::IoAtPath {
            path: dir.to_owned(),
            source: e,
        })?;
        let mut out = Vec::new();
        for item in rd {
            let item = match item {
                Ok(i) => i,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(Error::IoAtPath {
                        path: dir.to_owned(),
                        source: e,
                    });
                }
            };
            let name = item.file_name();
            let name = BString::from(name.as_os_str().as_bytes());
            if name == b".git" {
                continue;
            }
            let mut path_bytes = dir.to_vec();
            if !path_bytes.is_empty() {
                path_bytes.push(b'/');
            }
            path_bytes.extend_from_slice(&name[..]);
            let path = BString::from(path_bytes);
            let meta = match std::fs::symlink_metadata(item.path()) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(Error::IoAtPath {
                        path: path.clone(),
                        source: e,
                    });
                }
            };
            let ft = meta.file_type();
            let kind = if ft.is_dir() {
                PathKind::Directory
            } else if ft.is_file() || ft.is_symlink() {
                PathKind::File
            } else {
                PathKind::Other
            };
            let stat = StatData::from_metadata(&meta, self.filemode, self.symlinks)?;
            let ignored = ignore.is_ignored(path.as_ref(), kind)?;
            out.push(WorktreeEntry {
                path,
                kind,
                mode: stat.mode,
                size: meta.len(),
                ignored,
            });
        }
        Ok(out)
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
        let fs_path = self.join(path)?;
        match std::fs::symlink_metadata(&fs_path) {
            Ok(meta) => Ok(Some(StatData::from_metadata(
                &meta,
                self.filemode,
                self.symlinks,
            )?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::IoAtPath {
                path: path.to_owned(),
                source: e,
            }),
        }
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
        let fs_path = self.join(path)?;
        let meta = std::fs::symlink_metadata(&fs_path).map_err(|e| Error::IoAtPath {
            path: path.to_owned(),
            source: e,
        })?;
        if meta.file_type().is_symlink() && self.symlinks {
            let target = std::fs::read_link(&fs_path).map_err(|e| Error::IoAtPath {
                path: path.to_owned(),
                source: e,
            })?;
            return Ok(target.as_os_str().as_bytes().to_vec());
        }
        std::fs::read(&fs_path).map_err(|e| Error::IoAtPath {
            path: path.to_owned(),
            source: e,
        })
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
        self.hash_file_with(path, &mut || false)
    }

    /// Phase 4 — [`Worktree::hash_file`] with the answer to "does the index blob for this path
    /// contain a CR", which `text=auto` consults before normalising line endings.
    ///
    /// # Errors
    ///
    /// As [`Worktree::hash_file`].
    pub fn hash_file_with(
        &self,
        path: &BStr,
        index_has_cr: &mut dyn FnMut() -> bool,
    ) -> Result<crate::hash::ObjectId> {
        let mut buf = self.read_file(path)?;
        let is_link = self.symlinks
            && std::fs::symlink_metadata(self.join(path)?)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
        if !is_link {
            buf = self.clean_content(path, buf, index_has_cr, false)?;
        }
        // The header is part of the identity: Git hashes `<kind> <len>\0` followed by the body,
        // so hashing the bytes alone yields an id no blob in the repository can match.
        let (id, _) = hash_object(self.format, ObjectKind::Blob, &buf)?;
        Ok(id)
    }

    /// Phase 4 — the modification time of this work tree's index file.
    ///
    /// Used by the racy-git check, which has to know when the index was written to decide
    /// whether an entry whose stat data matches could have been modified in the same
    /// timestamp tick.
    ///
    /// # Caveats
    ///
    /// * **Best effort.** A bare repository, a work tree with no index yet, or a `.git` that
    ///   cannot be read yields `(0, 0)`. That disables the racy check instead of failing
    ///   `status`: the fallback is to hash the file, which is slower but always correct.
    /// * **Linked work trees are followed.** When `.git` is a file rather than a directory, its
    ///   `gitdir:` line is read so the per-work-tree index is found and not the main one's.
    pub fn index_mtime(&self) -> (u32, u32) {
        let Some(root) = self.root() else {
            return (0, 0);
        };
        let dot_git = root.join(".git");
        let git_dir = if dot_git.is_file() {
            match std::fs::read_to_string(&dot_git) {
                Ok(text) => {
                    let target = text.strip_prefix("gitdir:").map(str::trim).unwrap_or("");
                    let target = std::path::Path::new(target);
                    if target.is_absolute() {
                        target.to_path_buf()
                    } else {
                        root.join(target)
                    }
                }
                Err(_) => return (0, 0),
            }
        } else {
            dot_git
        };
        stat::index_mtime(&git_dir.join("index")).unwrap_or((0, 0))
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
        crate::worktree::status::compute(self, index, head_tree, odb, ignore, options)
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

impl Default for WorktreeWalkOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl WorktreeWalkOptions {
    /// Phase 4 — the options `status` uses: recursive, untracked files included, ignored ones
    /// not.
    pub fn new() -> Self {
        Self {
            include_ignored: false,
            recursive: true,
            prefix: BString::from(""),
        }
    }

    /// Phase 4 — set whether ignored files are collected.
    pub fn include_ignored(&mut self, on: bool) -> &mut Self {
        self.include_ignored = on;
        self
    }

    /// Phase 4 — restrict the walk to a subtree.
    pub fn prefix(&mut self, prefix: &BStr) -> &mut Self {
        self.prefix = prefix.to_owned();
        self
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
    if path.is_empty() {
        return Ok(());
    }
    let bytes: &[u8] = path.as_ref();
    if bytes.contains(&0) {
        return Err(Error::InvalidPath(path.to_owned()));
    }
    // An absolute path, POSIX or Windows-style.
    if bytes[0] == b'/' || (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
    {
        return Err(Error::InvalidPath(path.to_owned()));
    }
    let mut components: Vec<&[u8]> = Vec::new();
    for comp in bytes.split(|b| *b == b'/') {
        if comp.is_empty() {
            continue;
        }
        if comp == b".." {
            return Err(Error::InvalidPath(path.to_owned()));
        }
        components.push(comp);
    }
    if protect_ntfs {
        for comp in &components {
            if comp.iter().any(|b| *b == b':' || *b == b'\\') {
                return Err(Error::InvalidPath(path.to_owned()));
            }
            if matches!(comp.last(), Some(b'.') | Some(b' ')) {
                return Err(Error::InvalidPath(path.to_owned()));
            }
            let stem = comp.split(|b| *b == b'.').next().unwrap_or(&[]);
            let upper: Vec<u8> = stem.iter().map(|b| b.to_ascii_uppercase()).collect();
            let reserved = matches!(upper.as_slice(), b"CON" | b"PRN" | b"AUX" | b"NUL")
                || (upper.len() == 4
                    && (upper.starts_with(b"COM") || upper.starts_with(b"LPT"))
                    && upper[3].is_ascii_digit()
                    && upper[3] != b'0');
            if reserved {
                return Err(Error::InvalidPath(path.to_owned()));
            }
        }
    }
    Ok(())
}
