//! Phase 2 — loose refs: one small file per ref, under `refs/`.
//!
//! The original storage, and still the one every update writes. A loose ref file holds either a
//! single object id in hex and a newline, or a symbolic redirect:
//!
//! ```text
//! 3f2c1a9e…\n              <- a direct ref
//! ref: refs/heads/main\n   <- a symbolic ref
//! ```
//!
//! `HEAD` is the symbolic ref everybody meets first, but nothing is special about it: any ref may
//! be symbolic, and `refs/remotes/origin/HEAD` usually is.
//!
//! # Caveats that bite
//!
//! * **Loose shadows packed.** [`read`] looks only here, so a caller that gets
//!   [`crate::error::Error::RefNotFound`] has not yet learned that the ref does not exist — it may
//!   be in `packed-refs`. [`crate::refs::RefStore::read`] is the lookup that means "the ref's
//!   value"; this module is one half of it.
//! * **The trailing newline is optional on read and mandatory on write.** Git writes `\n` and
//!   accepts a file without one, because a file written by `echo -n` and a file truncated
//!   mid-write are indistinguishable except by length. Neither is `..`-dangerous, so both parse.
//! * **Whitespace around the value is not allowed.** Git's reader tolerates a trailing `\r` (a
//!   Windows checkout gone wrong) and rejects interior spaces. A parser that trims aggressively
//!   turns a corrupt file into a plausible one, which is worse than an error.
//! * **An empty file is a ref that exists and points nowhere.** Git reports this as a broken ref
//!   rather than as absence, and so must Rivet: `git fsck` distinguishes the two, and a branch whose
//!   file is empty still appears in `git branch`. [`read`] therefore raises an error for it rather
//!   than returning [`crate::error::Error::RefNotFound`].
//! * **Deletion must prune the directory tree**, or a repository accumulates empty `refs/heads/a/b/`
//!   directories that make `git branch`'s listing walk longer and that a subsequent creation of a
//!   ref named `refs/heads/a/b` would trip over — a directory cannot be created where one exists.
//! * **Writes are `<ref>.lock` then rename.** The lock is what makes a concurrent `git` see either
//!   the old value or the new one; a direct write would let it read a half-written file and treat
//!   the truncation as a shorter, wrong id.
//! * **Ref files are read with no size limit.** A ref file is tens of bytes in every real
//!   repository, but nothing in the format says so, and a corrupt or hostile one can be gigabytes.
//!   The implementation must bound the read rather than trusting the file's length.

use std::path::{Path, PathBuf};

use bstr::{BStr, BString};

use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};

use super::{RefLock, RefName, RefTarget};

/// Phase 2 — read a loose ref, without consulting `packed-refs`.
///
/// # Caveats
///
/// * **This is one half of a ref lookup.** A missing loose file does not mean the ref is missing;
///   use [`crate::refs::RefStore::read`] unless the caller genuinely wants to know about loose
///   storage specifically (which `gc` and `pack-refs` do).
/// * **A symbolic ref is returned as such, not followed.** `HEAD` reads as
///   [`RefTarget::Symbolic`]; resolving it to an id is [`crate::refs::RefStore::resolve_oid`]'s
///   job, which bounds the chain at [`crate::refs::MAX_SYMBOLIC_DEPTH`].
/// * **The value is parsed at the repository's format width.** A 40-hex value in a SHA-256
///   repository is a parse error here, not a valid SHA-1 id — an id's width is a property of the
///   repository, not of the string.
///
/// # Errors
///
/// [`crate::error::Error::RefNotFound`] if the file is absent;
/// [`crate::error::Error::IoAtPath`] if it exists but cannot be read;
/// [`crate::error::Error::InvalidHex`] for a value that is not a full-length hex id;
/// [`crate::error::Error::InvalidRefName`] for a `ref:` target that is not a valid ref name.
pub fn read(git_dir: &Path, name: &RefName, format: ObjectFormat) -> Result<RefTarget> {
    let path = name.file_path(git_dir);
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::RefNotFound {
                name: BString::from(name.as_bytes()),
            });
        }
        Err(e) => {
            return Err(Error::IoAtPath {
                path: path_bytes(&path),
                source: e,
            });
        }
    };
    let mut data = Vec::new();
    {
        use std::io::Read;
        let mut limited = (&mut file).take(MAX_REF_FILE + 1);
        limited
            .read_to_end(&mut data)
            .map_err(|e| Error::IoAtPath {
                path: path_bytes(&path),
                source: e,
            })?;
    }
    if data.len() as u64 > MAX_REF_FILE {
        return Err(Error::InvalidRefName {
            name: BString::from(name.as_bytes()),
            reason: "ref file is larger than any valid ref",
        });
    }
    parse_ref_file(format, &data)
}

/// The largest ref file Rivet will read. A ref is tens of bytes; anything past this is corruption
/// or a hostile file, and reading it unbounded is the one way a ref lookup can exhaust memory.
const MAX_REF_FILE: u64 = 4096;

/// A path as the raw bytes an error variant reports.
fn path_bytes(path: &Path) -> BString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        BString::from(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        BString::from(path.to_string_lossy().as_bytes())
    }
}

/// A relative path's bytes, for turning a walked path into a ref name.
fn rel_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn walk_refs(root: &Path, dir: &Path, out: &mut Vec<RefName>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(Error::IoAtPath {
                path: path_bytes(dir),
                source: e,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::IoAtPath {
            path: path_bytes(dir),
            source: e,
        })?;
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk_refs(root, &entry.path(), out)?;
        } else if file_type.is_file() {
            let path = entry.path();
            let rel = match path.strip_prefix(root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if let Ok(name) = RefName::from_bytes(&rel_bytes(rel)) {
                out.push(name);
            }
        }
    }
    Ok(())
}

/// Phase 2 — read a loose ref, treating absence as `None`.
///
/// The form every caller that is about to create the ref wants: "read the old value, if any" is a
/// compare-and-swap's setup and a `branch -m`'s existence check, and both treat absent as an answer
/// rather than as a failure.
///
/// # Caveats
///
/// `Ok(None)` means *the loose file is absent*. It does not mean the ref does not exist — see
/// [`crate::refs::RefStore::try_read`] for that question.
///
/// # Errors
///
/// Everything [`read`] raises except [`crate::error::Error::RefNotFound`].
pub fn try_read(git_dir: &Path, name: &RefName, format: ObjectFormat) -> Result<Option<RefTarget>> {
    read(git_dir, name, format).map(Some).or_else(|e| {
        if matches!(e, crate::error::Error::RefNotFound { .. }) {
            Ok(None)
        } else {
            Err(e)
        }
    })
}

/// Phase 2 — read a ref file that holds **one id per line**, such as `MERGE_HEAD`.
///
/// # Caveats
///
/// * `MERGE_HEAD` holds one line per parent a merge would add — normally one, but an octopus merge
///   writes several, and `FETCH_HEAD` uses a completely different line format (`<id>\t<desc>`) that
///   this must not be used for.
/// * Blank lines are skipped rather than treated as terminators: a merge in progress that a tool
///   appended a newline to must still resolve.
/// * An empty file yields an empty vector, not an error. "A merge is in progress" and "a merge is
///   in progress with an unreadable parent list" are different states, and only the caller knows
///   which is a failure.
///
/// # Errors
///
/// [`crate::error::Error::RefNotFound`] if the file is absent, and
/// [`crate::error::Error::InvalidHex`] for a line that is not a full-length id.
pub fn read_ids(git_dir: &Path, name: &RefName, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let path = name.file_path(git_dir);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::RefNotFound {
                name: BString::from(name.as_bytes()),
            });
        }
        Err(e) => {
            return Err(Error::IoAtPath {
                path: path_bytes(&path),
                source: e,
            });
        }
    };
    let mut out = Vec::new();
    for line in data.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        out.push(ObjectId::from_hex(format, BStr::new(line))?);
    }
    Ok(out)
}

/// Phase 2 — whether a loose ref file exists.
///
/// # Caveats
///
/// A `stat` on the file, so a ref whose file is empty, unreadable or corrupt answers `true`. That
/// is the right answer for the question it asks — Git distinguishes "no such ref" from "broken ref"
/// and so must Rivet — but it is not an existence check for the ref, which may also be packed.
pub fn exists(git_dir: &Path, name: &RefName) -> bool {
    name.file_path(git_dir).exists()
}

/// Phase 2 — write a loose ref, taking its lock and renaming into place.
///
/// # Caveats
///
/// * **Takes and releases the lock internally**, so a caller that needs to hold the lock across a
///   ref write *and* a reflog append — which is every update in [`crate::refs::RefStore`] — must use
///   [`RefLock`] directly. Doing this in two steps with this function in the middle leaves a window
///   where another process can move the ref between the write and the log line.
/// * **Parent directories are created.** `refs/heads/feature/x` needs `refs/heads/feature/`, which
///   may not exist; creating it is the only way to create the ref, and it must not fail when it is
///   already there.
/// * **A directory in the way is an error, not something to work around.** Creating
///   `refs/heads/a` when `refs/heads/a/b` exists cannot succeed — the filesystem has no room for
///   both — and Git refuses rather than deleting the subtree, because the subtree is other refs.
/// * **The lock file's name is not negotiable**: `<ref>.lock`, in the same directory, so that the
///   rename is atomic and so that a concurrent Git takes the same lock.
///
/// # Errors
///
/// [`crate::error::Error::LockHeld`] if `<ref>.lock` already exists, and
/// [`crate::error::Error::IoAtPath`] if a directory cannot be created or the rename fails.
pub fn write(git_dir: &Path, name: &RefName, target: &RefTarget) -> Result<()> {
    let mut lock = RefLock::acquire(git_dir, name)?;
    lock.write_and_commit(&target.to_bytes(target_format(target)))
}

/// Phase 2 — write a ref's file through a lock the caller already holds.
///
/// The sequencing [`crate::refs::RefStore::set_direct`] needs: lock, write, append the reflog line,
/// commit. Splitting it this way keeps the lock's lifetime visible in the caller rather than hidden
/// inside a function whose name says nothing about it.
///
/// # Caveats
///
/// * The lock must be for **this ref**, and it must not have been committed yet. Passing another
///   ref's lock writes this ref unprotected — the check is the caller's, because a [`RefLock`] does
///   not record which ref it is for.
/// * The content written is [`RefTarget::to_bytes`], which is what makes a symbolic ref and a direct
///   ref share one writer. Serialising here instead would be the second place the format is spelled
///   out.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if the write fails.
pub fn write_locked(lock: &mut RefLock, target: &RefTarget) -> Result<()> {
    let data = target.to_bytes(target_format(target));
    lock.write_and_commit(&data)
}

/// The width a target is serialised at: an id carries its own format, so a SHA-256 id is written as
/// 64 hex digits whatever repository the caller thinks it is in. A symbolic target has no width.
fn target_format(target: &RefTarget) -> ObjectFormat {
    match target {
        RefTarget::Direct(id) => id.format(),
        RefTarget::Symbolic(_) => ObjectFormat::Sha1,
    }
}

/// Phase 2 — delete a loose ref, pruning directories that become empty.
///
/// # Caveats
///
/// * **A missing file is not an error.** Deleting a ref that exists only in `packed-refs` is a
///   normal part of a deletion that removes both halves, and the caller should not have to check
///   which half exists first.
/// * **The reflog is not touched.** [`super::reflog::delete`] is a separate call on purpose: a
///   deletion that is really a move (an `-m` rename) wants to move the log, not drop it, and a
///   single function doing both would make that impossible.
/// * **Empty parents are pruned upward**, but a directory that still holds another ref — or a file
///   that is not a ref — must be left alone. Pruning only stops at a non-empty directory, never
///   recurses into a sibling.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if the file exists and cannot be removed. A failure to prune a
/// directory afterwards is **not** an error: the ref is gone either way, and an empty directory is
/// inert.
pub fn delete(git_dir: &Path, name: &RefName) -> Result<()> {
    let path = name.file_path(git_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::IoAtPath {
                path: path_bytes(&path),
                source: e,
            });
        }
    }
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.is_dir() {
            match std::fs::read_dir(d) {
                Ok(mut entries) => {
                    if entries.next().is_none() {
                        let _ = std::fs::remove_dir(d);
                    } else {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        dir = d.parent();
    }
    Ok(())
}

/// Phase 2 — move a loose ref, taking both locks.
///
/// Backs `branch -m` and the internal ref moves a fetch performs. Tags are not renamed in Git, so
/// a tag rename has no caller here.
///
/// # Caveats
///
/// * **Both names are locked**, source first, and the locks are held across the delete and the
///   write. Locking only the destination leaves a window in which a third process creates the
///   destination and this move then overwrites it.
/// * **The reflog is not moved.** Git appends a `Branch: renamed refs/heads/a to refs/heads/b` line
///   to the *new* ref's log and leaves the old log in place (it is reachable as `a@{n}` until
///   expiry). Moving the file instead would lose the "created from" line and change what
///   `@{1}` means, so it is deliberately not done here.
/// * **A packed source is not moved.** If the ref exists only in `packed-refs`, this deletes
///   nothing loose and writes the new loose ref; the packed entry keeps the old name until the next
///   `pack-refs`. That is Git's behaviour too, and it is why a rename must also update
///   `packed-refs` when it has one.
///
/// # Errors
///
/// [`crate::error::Error::LockHeld`] for either lock,
/// [`crate::error::Error::RefNotFound`] if the source has no loose file and
/// [`crate::error::Error::IoAtPath`] if the write or the removal fails.
pub fn rename(git_dir: &Path, from: &RefName, to: &RefName) -> Result<()> {
    let mut src_lock = RefLock::acquire(git_dir, from)?;
    let mut dst_lock = RefLock::acquire(git_dir, to)?;
    // The repository's format is not a parameter, so take it from the file: a 64-digit id is
    // rejected as the wrong width by the SHA-1 parse and accepted by the SHA-256 one.
    let target = match read(git_dir, from, ObjectFormat::Sha1) {
        Err(Error::InvalidHex { .. }) => read(git_dir, from, ObjectFormat::Sha256)?,
        other => other?,
    };
    delete(git_dir, from)?;
    write_locked(&mut dst_lock, &target)?;
    let _ = src_lock.release();
    let _ = dst_lock.release();
    Ok(())
}

/// Phase 2 — the path of a ref's lock file.
///
/// # Caveats
///
/// Just the ref's path with `.lock` appended, in the ref's own directory — **not** a shared lock
/// directory. A `RefName` cannot itself end in `.lock` (validation forbids it), which is what keeps
/// `refs/heads/x.lock` and the lock of `refs/heads/x` from being the same path.
pub fn lock_path(git_dir: &Path, name: &RefName) -> PathBuf {
    let mut path = name.file_path(git_dir).into_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

/// Phase 2 — every loose ref whose name begins with `prefix`.
///
/// # Caveats
///
/// * **A recursive walk of `refs/`, plus the pseudo-refs at the top of the git directory** —
///   `HEAD`, `FETCH_HEAD`, `MERGE_HEAD` and friends. Whether those belong in a listing depends on
///   the caller (`git branch -a` excludes them, `git for-each-ref` includes them with a
///   `--include-root-refs` flag), so they are returned and the caller filters.
/// * **Symlinks are not followed.** A `.git/refs/heads/x` that is a symlink to a file outside the
///   git directory would otherwise let a repository read arbitrary files, and Git refuses to follow
///   them for exactly that reason.
/// * **Files that are not valid ref names are skipped, not reported.** A `refs/heads/x.lock` left
///   by a crashed process, a `.DS_Store`, and a directory named `refs/heads/zh` in a hostile
///   repository all fail validation; failing the whole listing on one of them makes a repository
///   that Git lists fine unlistable by Rivet.
/// * **Sorted by name.** The walk is not in any guaranteed order, and a caller that compares
///   listings between runs — a differential test against `git for-each-ref` — needs one.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if `refs/` or a subdirectory cannot be listed, and the
/// per-file errors of [`read`] for a file that is listed and then does not parse.
pub fn list(
    git_dir: &Path,
    prefix: &BStr,
    format: ObjectFormat,
) -> Result<Vec<(RefName, RefTarget)>> {
    let mut out = Vec::new();
    for name in list_names(git_dir, prefix)? {
        let target = read(git_dir, &name, format)?;
        out.push((name, target));
    }
    Ok(out)
}

/// Phase 2 — the names, without reading the values, of every loose ref under `prefix`.
///
/// # Caveats
///
/// The names come from the **filesystem walk**, so a ref file that is corrupt, empty or holds a
/// name that fails validation still appears here if its path is a valid ref name. That asymmetry
/// with [`list`] is deliberate: this backs listings such as `branch --list` that must show a
/// branch Git shows even when its value is broken.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if a directory cannot be listed.
pub fn list_names(git_dir: &Path, prefix: &BStr) -> Result<Vec<RefName>> {
    let mut names = Vec::new();
    walk_refs(git_dir, &git_dir.join("refs"), &mut names)?;
    if let Ok(entries) = std::fs::read_dir(git_dir) {
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            let rel = match path.strip_prefix(git_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if let Ok(name) = RefName::from_bytes(&rel_bytes(rel)) {
                names.push(name);
            }
        }
    }
    names.retain(|n| n.as_bytes().starts_with(prefix.as_ref()));
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    names.dedup();
    Ok(names)
}

/// Phase 2 — parse a ref file's contents into a target.
///
/// Split out so that the same parser serves a ref file, a `.lock` file being verified, and the
/// scratch buffer in [`read`] that is read with a size bound.
///
/// # Caveats
///
/// * The `ref: ` prefix is matched **exactly**, with one space after the colon, as Git writes it. A
///   file containing `ref:refs/heads/main` is corruption in Git's eyes and must not be accepted
///   here, or the two tools disagree about which refs are valid.
/// * A target that is itself a symbolic ref is legal: `refs/remotes/origin/HEAD` pointing at
///   `refs/remotes/origin/main`, and `HEAD` pointing at a symbolic ref, are both ordinary.
/// * Trailing whitespace: a single trailing `\n` and a single trailing `\r\n` are stripped; any
///   other trailing whitespace is part of the value and makes it invalid. Trimming more would
///   accept files Git rejects.
///
/// # Errors
///
/// [`crate::error::Error::InvalidHex`] for a direct value that is not a full-length id,
/// [`crate::error::Error::InvalidRefName`] for a symbolic target or a file that holds nothing —
/// an empty ref file is a ref that exists and points nowhere, which is an error and not absence.
pub fn parse_ref_file(format: ObjectFormat, data: &[u8]) -> Result<RefTarget> {
    let mut value = data;
    if let Some(rest) = value.strip_suffix(b"\n") {
        value = rest;
        if let Some(rest) = value.strip_suffix(b"\r") {
            value = rest;
        }
    }
    if value.is_empty() {
        return Err(Error::InvalidRefName {
            name: BString::new(Vec::new()),
            reason: "ref file is empty",
        });
    }
    if let Some(rest) = value.strip_prefix(b"ref: ") {
        return Ok(RefTarget::Symbolic(RefName::parse(BStr::new(rest))?));
    }
    Ok(RefTarget::Direct(ObjectId::from_hex(
        format,
        BStr::new(value),
    )?))
}

/// Phase 2 — the id a ref file holds, requiring it to be direct.
///
/// # Caveats
///
/// A symbolic ref is [`crate::error::Error::SymbolicRef`] rather than an implicit follow. Following
/// it would need the ref store, and a function in this module that half-resolves would be the place
/// a `HEAD` pointing at a missing branch returned a stale id.
///
/// # Errors
///
/// [`crate::error::Error::SymbolicRef`] if the value is a symbolic redirect, plus everything
/// [`read`] raises.
pub fn read_direct(git_dir: &Path, name: &RefName, format: ObjectFormat) -> Result<ObjectId> {
    match read(git_dir, name, format)? {
        RefTarget::Direct(oid) => Ok(oid),
        RefTarget::Symbolic(_) => Err(Error::SymbolicRef {
            name: BString::from(name.as_bytes()),
        }),
    }
}

/// Phase 2 — the symbolic target of a ref file, requiring it to be symbolic.
///
/// Backs `git symbolic-ref` and the `switch` path that needs to know which branch `HEAD` names.
/// The mirror of [`read_direct`], with the opposite error.
///
/// # Errors
///
/// [`crate::error::Error::SymbolicRef`] if the value is a direct id, plus everything [`read`]
/// raises.
pub fn read_symbolic(git_dir: &Path, name: &RefName, format: ObjectFormat) -> Result<BString> {
    match read(git_dir, name, format)? {
        RefTarget::Symbolic(target) => Ok(target.into_bstring()),
        RefTarget::Direct(oid) => Err(Error::SymbolicRef { name: oid.to_hex() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> RefName {
        RefName::from_bytes(n.as_bytes()).unwrap()
    }

    fn sha256_id(byte: u8) -> ObjectId {
        let hex = format!("{byte:02x}").repeat(32);
        ObjectId::from_hex(ObjectFormat::Sha256, hex.as_bytes().into()).unwrap()
    }

    #[test]
    fn write_locked_and_rename_carry_a_sha256_id_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (name("refs/heads/a"), name("refs/heads/b"));
        let id = sha256_id(0xab);
        write(dir.path(), &a, &RefTarget::Direct(id)).unwrap();
        let on_disk = std::fs::read(a.file_path(dir.path())).unwrap();
        assert_eq!(on_disk, format!("{}\n", "ab".repeat(32)).into_bytes());

        rename(dir.path(), &a, &b).unwrap();
        assert!(!exists(dir.path(), &a));
        assert_eq!(
            read(dir.path(), &b, ObjectFormat::Sha256).unwrap(),
            RefTarget::Direct(id)
        );
    }

    #[test]
    fn sha1_ids_are_still_written_at_forty_digits() {
        let dir = tempfile::tempdir().unwrap();
        let r = name("refs/heads/a");
        let id = ObjectId::from_hex(ObjectFormat::Sha1, "cd".repeat(20).as_bytes().into()).unwrap();
        write(dir.path(), &r, &RefTarget::Direct(id)).unwrap();
        rename(dir.path(), &r, &name("refs/heads/c")).unwrap();
        let on_disk = std::fs::read(name("refs/heads/c").file_path(dir.path())).unwrap();
        assert_eq!(on_disk, format!("{}\n", "cd".repeat(20)).into_bytes());
    }
}
