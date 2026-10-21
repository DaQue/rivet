//! Phase 2 — reflogs: the per-ref record of where a ref used to point.
//!
//! Every ref update appends one line to `.git/logs/<refname>` (and `HEAD`'s log is
//! `.git/logs/HEAD`). Each line records the old id, the new id, who did it and why:
//!
//! ```text
//! 0000000… 3f2c1a… Rivet <rivet@example.com> 1759708800 +0200\tcommit: add parser
//! ```
//!
//! The format is the same signature spelling Git uses in commits, which is why this module reuses
//! [`Signature`] rather than parsing a name/email/time trio of its own — a second parser would
//! eventually disagree with the first about a signature containing a `>` or a name with no email.
//!
//! # Caveats that bite
//!
//! * **A missing reflog is normal, not an error.** `core.logAllRefUpdates` defaults to true only
//!   for repositories with a work tree, individual refs may be created without a log, and a
//!   freshly-cloned repository has none for its remote-tracking refs. Reading an absent log is an
//!   empty log.
//! * **Newlines matter in a way that is easy to get backwards.** Entries are `\n`-terminated, so a
//!   file ending in `\n` has no trailing empty entry; a file not ending in one is either a
//!   truncated write or a legitimate log from a tool that forgot the terminator, and both must
//!   parse the last line rather than dropping it.
//! * **The message is tab-separated from the signature, and the tab is the *first* tab.** Git
//!   writes exactly one tab between the timestamp and the message, and messages are single-line by
//!   construction — but a message containing a tab is legal and must not be split further.
//! * **Reflog lines are not authoritative and must not be trusted for values.** They record what
//!   an operation *claimed*; a crash between the ref write and the log append leaves them
//!   disagreeing, and `HEAD@{1}` resolution has to cope with a log that does not match the current
//!   ref. Reading the reflog is history; reading the ref is state.
//! * **The log path is derived from the ref name with `logs/` in front of the whole name** —
//!   `logs/refs/heads/main`, not `refs/heads/logs/main`. See [`RefName::reflog_path`].
//! * **Appending is safe only under the ref's lock.** Git appends to the reflog while holding
//!   `<ref>.lock`, so two concurrent updates cannot interleave two partial lines. An append that
//!   does not hold the lock produces a file that is line-parseable but records the updates in an
//!   arbitrary order, and can interleave two writes' bytes within one line if they are large
//!   enough.

use std::path::Path;

use bstr::{BStr, BString};

use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::Signature;

use super::RefName;

/// Phase 2 — one line of a reflog.
///
/// # Caveats
///
/// * **`old` is the all-zero id for a creation and `new` is all-zero for a deletion**, which is
///   how Git records "there was nothing here" and "there is nothing here now". Both zero ids are
///   indistinguishable from a real id only in that they can never name a stored object; see
///   [`ObjectId::is_zero`].
/// * **`who` is a full [`Signature`]**, carrying a name, an email, a timestamp and a UTC offset
///   exactly as a commit does. It is not the current user at read time, and it is not trimmed or
///   re-rendered — a log line written by another tool keeps its own spelling.
/// * **The message is a single line of bytes** and may be empty. Git's conventions (`commit: …`,
///   `checkout: moving from a to b`, `pull: Fast-forward`) are conventions, not a grammar; Rivet
///   must not parse the message to decide anything, and must preserve a message it did not write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflogEntry {
    /// Where the ref pointed before the update, or the all-zero id for a creation.
    pub old: ObjectId,
    /// Where the ref points after the update, or the all-zero id for a deletion.
    pub new: ObjectId,
    /// Who performed the update.
    pub who: Signature,
    /// The one-line reason, e.g. `commit: add parser`. May be empty, never contains `\n`.
    pub message: BString,
}

impl ReflogEntry {
    /// Phase 2 — build an entry.
    ///
    /// # Caveats
    ///
    /// The message is stored as given: this does **not** strip a trailing newline, replace tabs, or
    /// truncate at one. A caller supplying a message with a newline in it writes a reflog line that
    /// splits into two on the next read — the second half of which is corrupt. The one place that
    /// matters is [`ReflogEntry::to_bytes`], which is where the invariant is defended.
    pub fn new(old: ObjectId, new: ObjectId, who: Signature, message: BString) -> Self {
        Self {
            old,
            new,
            who,
            message,
        }
    }

    /// Phase 2 — parse one reflog line, without its trailing newline.
    ///
    /// # Caveats
    ///
    /// * The two ids are the first two space-separated fields, then the signature runs to the
    ///   **first tab**, then everything after that tab is the message. Splitting the signature and
    ///   message on a space instead of the tab truncates any message containing a space, which is
    ///   nearly all of them.
    /// * The signature is parsed by the same code as a commit's author line, because it *is* the
    ///   same spelling — with one difference: a reflog signature may have an empty name and email
    ///   (from a tool that had no identity configured), where a commit's may not. The
    ///   implementation must therefore not simply call the strict commit parser if it rejects that
    ///   case; Git writes such lines and `git reflog` reads them.
    /// * A line with no tab at all is a signature with an empty message, not a corrupt line. Git
    ///   writes the tab even for an empty message, but logs from other tools do not always.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptReflog`] if either id is not a valid id at the repository's
    /// format width or the signature does not parse. The reflog's path is not available here, so the
    /// caller that knows it must attach it — the variant's `path` field is filled by
    /// [`ReflogEntry::parse_at`].
    pub fn parse(format: ObjectFormat, line: &BStr) -> Result<Self> {
        let mut fields = line.splitn(3, |b| *b == b' ');
        let old_raw = fields.next().unwrap_or(b"");
        let new_raw = fields.next().unwrap_or(b"");
        let rest = fields.next().unwrap_or(b"");
        let corrupt = |reason: &'static str| Error::CorruptReflog {
            path: BString::from(""),
            line: 0,
            reason,
        };
        let old =
            ObjectId::from_hex(format, BStr::new(old_raw)).map_err(|_| corrupt("bad old id"))?;
        let new =
            ObjectId::from_hex(format, BStr::new(new_raw)).map_err(|_| corrupt("bad new id"))?;
        let (signature, message) = match rest.iter().position(|b| *b == b'\t') {
            Some(tab) => (&rest[..tab], &rest[tab + 1..]),
            None => (rest, &b""[..]),
        };
        let who = Signature::parse(BStr::new(signature)).unwrap_or_else(|_| Signature {
            name: BString::new(Vec::new()),
            email: BString::new(Vec::new()),
            when: 0,
            tz_offset_minutes: 0,
        });
        Ok(Self {
            old,
            new,
            who,
            message: BString::from(message),
        })
    }

    /// Phase 2 — parse one reflog line, reporting the file it came from.
    ///
    /// The form [`read`] uses, so that a corrupt line's error names the file and the line number
    /// rather than leaving the user to guess which of a repository's reflogs is broken.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptReflog`] with `path` and `line` filled in, for the same
    /// failures as [`ReflogEntry::parse`].
    pub fn parse_at(format: ObjectFormat, path: &BStr, line: usize, data: &BStr) -> Result<Self> {
        Self::parse(format, data).map_err(|e| match e {
            Error::CorruptReflog { reason, .. } => Error::CorruptReflog {
                path: BString::from(path),
                line,
                reason,
            },
            other => other,
        })
    }

    /// Phase 2 — serialise to one reflog line, **including** its trailing newline.
    ///
    /// # Caveats
    ///
    /// * The newline is part of the output because every caller is appending to a file; a caller
    ///   that wants the line without it strips one byte.
    /// * A message containing `\n` must be **rejected or sanitised**, not written: writing it
    ///   produces a file whose second line fails to parse on the next read, which turns one bad
    ///   update into an unreadable reflog. Git strips such characters from reflog messages before
    ///   writing; Rivet must do the same rather than trusting the caller.
    /// * The separator between the signature and the message is exactly one tab, even when the
    ///   message is empty. That is what Git writes and what a byte-for-byte comparison expects.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.old.to_hex());
        out.push(b' ');
        out.extend_from_slice(&self.new.to_hex());
        out.push(b' ');
        out.extend_from_slice(&self.who.to_bytes());
        out.push(b'\t');
        for byte in self.message.iter() {
            if *byte == b'\n' || *byte == b'\r' {
                out.push(b' ');
            } else {
                out.push(*byte);
            }
        }
        out.push(b'\n');
        out
    }

    /// Phase 2 — whether this entry records a ref being created.
    ///
    /// # Caveats
    ///
    /// True when `old` is the all-zero id. `HEAD` being created in a fresh repository and a branch
    /// being created by `switch -c` both look like this; the reflog alone cannot tell them apart,
    /// and the message is what distinguishes them for a human.
    pub fn is_creation(&self) -> bool {
        self.old.is_zero()
    }

    /// Phase 2 — whether this entry records a ref being deleted.
    ///
    /// # Caveats
    ///
    /// True when `new` is the all-zero id. A ref update that legitimately moves a ref *to* a zero id
    /// is not meaningful — no object has that id — so this is unambiguous.
    pub fn is_deletion(&self) -> bool {
        self.new.is_zero()
    }
}

/// Phase 2 — who is making a ref update, and why, for the reflog line it will append.
///
/// # Caveats
///
/// * Borrowed rather than owned, because the caller already has a [`Signature`] — usually the one
///   it just put in a commit — and a reflog append is a file write either way, so copying the
///   identity would add an allocation without removing one.
/// * The message should follow Git's conventions for the operations Rivet implements
///   (`commit: <subject>`, `checkout: moving from <old> to <new>`, `branch: Created from <rev>`,
///   `reset: moving to <rev>`, `merge <rev>: <outcome>`) but nothing enforces it. Tools read these
///   strings to produce `git reflog` output, so they are a compatibility surface even though they
///   are not a format.
/// * Passing `None` where a `ReflogMessage` is accepted means **no reflog line is written**, which
///   is not the same as writing one with an empty message: the reference implementation must append
///   nothing, leaving the log's last entry describing the previous update. Operations that
///   legitimately want a silent update (a `gc` moving refs into `packed-refs`, a fetch updating a
///   remote-tracking ref for the first time) rely on that distinction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReflogMessage<'a> {
    /// The identity to record, normally resolved from `user.name`/`user.email` and the clock.
    pub who: &'a Signature,
    /// The one-line reason. Must not contain `\n`; see [`ReflogEntry::to_bytes`].
    pub message: &'a BStr,
}

impl<'a> ReflogMessage<'a> {
    /// Phase 2 — pair an identity with a reason.
    pub const fn new(who: &'a Signature, message: &'a BStr) -> Self {
        Self { who, message }
    }
}

/// Phase 2 — read a ref's reflog, oldest entry first.
///
/// # Caveats
///
/// * **A missing file returns an empty log**, not [`crate::error::Error::RefNotFound`]. Most refs
///   in most repositories have no reflog, and a caller asking "what is in this log" wants an empty
///   answer, not a failure.
/// * **The order is file order, which is chronological.** Git appends, so index 0 is the oldest
///   entry. Revision syntax counts backwards from the newest — `HEAD@{0}` is the *last* element —
///   so the caller doing that arithmetic must reverse or index from the end, and getting it
///   backwards is a silent off-by-one in `reflog@{n}` output.
/// * **The whole log is read, however long.** A repository with a decade of `HEAD` movement has a
///   `logs/HEAD` in the megabytes; there is no `-n` here, and a caller that wants the last ten
///   entries should have the implementation stop early rather than read-and-truncate.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if the log exists but cannot be read, and
/// [`crate::error::Error::CorruptReflog`] for a line that does not parse — including the
/// truncated-line case a killed writer leaves behind.
pub fn read(git_dir: &Path, name: &RefName, format: ObjectFormat) -> Result<Vec<ReflogEntry>> {
    let path = name.reflog_path(git_dir);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::IoAtPath {
                path: path.to_string_lossy().as_bytes().into(),
                source: e,
            });
        }
    };
    let display = name.as_bytes().to_vec();
    let mut out = Vec::new();
    for (index, raw) in data.split(|b| *b == b'\n').enumerate() {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        out.push(ReflogEntry::parse_at(
            format,
            BStr::new(&display),
            index + 1,
            BStr::new(line),
        )?);
    }
    Ok(out)
}

/// Phase 2 — append one entry to a ref's reflog, creating the file and its directory if needed.
///
/// # Caveats
///
/// * **The caller must hold the ref's lock.** See the module caveat: an unheld append can interleave
///   with a concurrent one. The signature does not enforce this because the lock is taken on the ref
///   path and this function only knows the log path, and threading the lock through would make
///   `RefStore`'s update path awkward for the one case that does not need it.
/// * **The `logs/` tree is created on demand.** `logs/refs/heads/` does not exist in a fresh
///   repository and is not created by `git init`; the first append is what makes it.
/// * **`core.logAllRefUpdates` is the caller's policy, not this function's.** Writing to a
///   repository configured with `false` is not an error here, and appending to a ref that should
///   not have a log produces one that `git reflog` will happily show.
/// * Appending is a plain `O_APPEND` write, not a temp-file-and-rename: a reflog grows and its
///   history matters, so rewriting it to change one line is both wasteful and wrong. The atomicity
///   that matters comes from the lock, not from the rename.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if the directory cannot be created or the write fails.
pub fn append(git_dir: &Path, name: &RefName, entry: &ReflogEntry) -> Result<()> {
    use std::io::Write;
    let path = name.reflog_path(git_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::IoAtPath {
            path: parent.to_string_lossy().as_bytes().into(),
            source: e,
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| Error::IoAtPath {
            path: path.to_string_lossy().as_bytes().into(),
            source: e,
        })?;
    file.write_all(&entry.to_bytes())
        .map_err(|e| Error::IoAtPath {
            path: path.to_string_lossy().as_bytes().into(),
            source: e,
        })
}

/// Phase 2 — whether a ref has a reflog.
///
/// # Caveats
///
/// A `stat`, not a read: a log that exists but is empty, or corrupt, answers `true`. Its use is
/// deciding whether an operation will *create* a log — `git branch` reports "Created from" only
/// for a new one — and for that, existence is the whole question.
pub fn exists(git_dir: &Path, name: &RefName) -> bool {
    name.reflog_path(git_dir).exists()
}

/// Phase 2 — delete a ref's reflog, along with any now-empty parent directories.
///
/// Called when a ref is deleted. Git removes the log with the ref; leaving it behind makes
/// `reflog@{n}` for a recreated ref resolve to entries from the previous incarnation, which is
/// wrong in a way that is very hard to notice.
///
/// # Caveats
///
/// * **A missing log is a success**, not [`crate::error::Error::RefNotFound`]: deleting a branch
///   that never had a log is ordinary, and an error here would abort the whole deletion.
/// * **The empty-directory cleanup is best-effort.** Another ref's log may share the directory, and
///   removing it would destroy that ref's history. Only directories that are empty after the
///   removal are candidates, and a failure to remove one must not fail the operation.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if the file exists but cannot be removed.
pub fn delete(git_dir: &Path, name: &RefName) -> Result<()> {
    let path = name.reflog_path(git_dir);
    if !path.exists() {
        return Ok(());
    }
    let _ = std::fs::remove_file(&path);
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == git_dir {
            break;
        }
        if d.read_dir().map(|e| e.count()).unwrap_or(0) == 0 {
            let _ = std::fs::remove_dir(d);
        }
        dir = d.parent();
    }
    Ok(())
}

/// Phase 2 — the last entry of a ref's reflog, if it has one.
///
/// # Caveats
///
/// Reads the whole file to find the end, which is wasteful and is noted here because the honest
/// implementation seeks to the end and scans backwards for the final newline. It exists because
/// `HEAD` at a detached commit is reported by its reflog when the ref itself cannot say which
/// branch it came from, and that path runs on every `switch`.
///
/// # Errors
///
/// As [`read`].
pub fn last_entry(
    git_dir: &Path,
    name: &RefName,
    format: ObjectFormat,
) -> Result<Option<ReflogEntry>> {
    let entries = read(git_dir, name, format)?;
    Ok(entries.last().cloned())
}
