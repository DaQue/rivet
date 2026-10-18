//! Phase 2 — ref names: validation, classification, and the paths they map to.
//!
//! A ref name is a byte string that doubles as a filesystem path relative to the git directory
//! (`refs/heads/main` is the file `.git/refs/heads/main`) and as a component of the wire protocol
//! (`ls-refs` returns them wrapped in pkt-lines). Both roles impose constraints, and Git's
//! `check-ref-format` rules are the union of them: a name that would escape the git directory, or
//! that cannot be spelled in a pkt-line without ambiguity, is rejected when the ref is *created*
//! rather than when it is read.
//!
//! # Why the name is validated, not just parsed
//!
//! Every place a ref name reaches takes it as a [`RefName`], so validation happens once at the
//! boundary and no code downstream has to re-check. This matters most for the two dangerous
//! shapes: a name containing `..` or a leading `/`, which would make [`RefName::file_path`] produce
//! a path outside the git directory, and a name ending in `.lock`, which would collide with the
//! lock file of a legitimate ref and let one ref's update block another's.
//!
//! # Caveats that bite
//!
//! * **Names are bytes, not `String`.** Git stores ref names as raw bytes on disk, and on Unix a
//!   branch may genuinely be named with non-UTF-8 bytes. `BString` throughout; the one place the
//!   conversion is unavoidable is [`RefName::file_path`], and it is a documented lossy edge there.
//! * **`RefName` is owned, not borrowed.** Everywhere else in this crate a parsed value borrows its
//!   input, but a ref name outlives the buffer it came from: it is read out of a loose-ref file, a
//!   `packed-refs` line, or the contents of `HEAD`, and then handed to the caller as the answer to
//!   "which branch is this". Threading a lifetime through [`crate::refs::RefTarget`] and
//!   [`crate::refs::RefStore`] to save a few-byte allocation on an operation that touches the
//!   filesystem is not a trade worth making.
//! * **Case is meaningful, and so is the filesystem's opinion.** Once a ref is written, its case in
//!   the name is preserved verbatim, but on a case-insensitive filesystem `refs/heads/Main` and
//!   `refs/heads/main` are the same file. Git handles this by refusing creation of a ref that
//!   differs only in case from an existing one; Rivet must do the same rather than let the
//!   filesystem silently alias them.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use bstr::{BStr, BString};

use crate::error::Result;

/// Phase 2 — the literal name of the ref that holds the current branch, or a direct commit id.
///
/// # Caveats
///
/// `HEAD` is deliberately not under `refs/`, and it is the only one-level name Rivet writes. It is
/// a ref like any other for reading — it can be symbolic, it has a reflog at `logs/HEAD`, and it
/// can be locked and updated through the same code path — so code that special-cases it is usually
/// code that has confused "the ref the user is on" with "a ref".
pub const HEAD: &[u8] = b"HEAD";

/// Phase 2 — which namespace a ref name lives in.
///
/// # Caveats
///
/// This is a *classification of the name*, not a lookup: a name under `refs/tags/` is
/// [`RefCategory::Tag`] whether or not anything is stored there. Deciding what a ref points at is
/// [`crate::refs::RefStore`]'s job and needs the object database; deciding what kind of ref a name
/// is needs nothing but the name.
///
/// [`RefCategory::Other`] is a real answer and not a fallback to be ignored: `refs/notes/`,
/// `refs/stash`, `refs/bisect/*` and anything a user invents all live under `refs/`, and a listing
/// that dropped them would break `git log --all`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RefCategory {
    /// The literal `HEAD` — the ref the work tree reflects.
    Head,
    /// `refs/heads/*` — a local branch.
    Branch,
    /// `refs/tags/*` — a lightweight or annotated tag.
    Tag,
    /// `refs/remotes/*` — a remote-tracking branch, updated by fetch and never by checkout.
    Remote,
    /// `refs/notes/*` — a notes tree, attached to a commit id rather than to a name.
    Note,
    /// Any other name: a pseudo-ref such as `MERGE_HEAD`, or a namespace Rivet does not model.
    Other,
}

impl RefCategory {
    /// Phase 2 — the on-disk namespace prefix, without a trailing slash.
    ///
    /// # Caveats
    ///
    /// `None` for [`RefCategory::Head`] and [`RefCategory::Other`], because neither corresponds to
    /// a prefix: `HEAD` sits at the top of the git directory and `Other` is defined by *not* being
    /// one of the namespaces. A caller that wants to build a name must handle both cases rather
    /// than concatenating an empty prefix and producing `/main`.
    pub const fn prefix(self) -> Option<&'static [u8]> {
        match self {
            RefCategory::Branch => Some(b"refs/heads"),
            RefCategory::Tag => Some(b"refs/tags"),
            RefCategory::Remote => Some(b"refs/remotes"),
            RefCategory::Note => Some(b"refs/notes"),
            RefCategory::Head | RefCategory::Other => None,
        }
    }

    /// Phase 2 — the short label, for CLI output and error messages.
    ///
    /// # Caveats
    ///
    /// Lowercase and singular: `"head"`, `"branch"`, `"tag"`, `"remote"`, `"note"`, `"other"`.
    /// This is Rivet's own vocabulary for reporting, not part of any on-disk format, so it may be
    /// changed for readability — unlike [`crate::object::ObjectKind::as_str`], which may not.
    pub const fn as_str(self) -> &'static str {
        match self {
            RefCategory::Head => "head",
            RefCategory::Branch => "branch",
            RefCategory::Tag => "tag",
            RefCategory::Remote => "remote",
            RefCategory::Note => "note",
            RefCategory::Other => "other",
        }
    }
}

/// Phase 2 — a validated ref name.
///
/// Constructing one runs Git's `check-ref-format` rules in full, so any `RefName` in hand is
/// guaranteed to be usable as a path under the git directory, as a reflog filename, and as a
/// pkt-line payload.
///
/// # Caveats
///
/// * **The name is stored exactly as given**, with no normalisation of case, no collapsing of
///   separators, and no trimming. `refs/heads/main` and `refs/heads/main/` are different strings,
///   and the second is invalid rather than equivalent — Git would create a *directory* for it.
/// * **An abbreviated name is not a ref name.** `main` is what a user types; `refs/heads/main` is
///   what the repository stores. Resolving the first to the second is revision parsing
///   (`Phase 4`), which needs the ref list and Git's disambiguation order (exact match, then
///   `refs/<name>`, then `refs/tags/<name>`, then `refs/heads/<name>`, …). It must not be done
///   here, because the answer depends on what exists.
/// * **Ordering is byte order.** `Ord` is derived so a listing can be sorted deterministically;
///   it is not a meaningful ordering of branches, and it is *not* the same order as Git's ref
///   sorting under `versionsort` config.
/// * **Validation is creation-time only for the rules Git checks that way.** A repository can
///   contain a ref file whose name violates these rules (Git has tightened them over the years
///   and older repositories remain readable). Reading such a name with [`RefName::parse`] fails,
///   which is deliberate — Rivet must not operate on a ref it would refuse to create — but it
///   means a `list` over a hostile repository can fail where Git's would warn.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefName(BString);

impl RefName {
    /// Phase 2 — validate a ref name.
    ///
    /// # Caveats
    ///
    /// The rules, all of which Git enforces on creation and this enforces always:
    ///
    /// * not empty; not exactly `@` (which is a Git shorthand for `HEAD` in *revision* syntax, not
    ///   a ref name)
    /// * no ASCII control characters, no space, no DEL, and none of `~ ^ : ? * [ \`
    /// * no `..` sequence and no `@{` sequence anywhere
    /// * no component beginning with `.`, no component ending with `.lock`, no component that is
    ///   the empty string (so no `//`, no leading `/`, no trailing `/`)
    /// * does not end with `.`
    /// * **at least one `/`**, unless the name is a one-level all-caps pseudo-ref or `HEAD` — this
    ///   is what keeps a bare `main` from becoming a file called `.git/main`
    ///
    /// The `@{` rule exists because revision syntax uses `@{` — `HEAD@{1}` — and a ref name that
    /// contained it would be unparseable by the revision parser that has to read names back.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRefName`] with the name and the specific rule broken. The
    /// name is reported as given: it is bytes, it may not be valid UTF-8, and a lossy conversion
    /// for the sake of an error message would misreport it.
    pub fn parse(name: &BStr) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — validate a ref name from raw bytes.
    ///
    /// The path a name takes when it comes off disk or off the wire, where it is a byte slice.
    ///
    /// # Errors
    ///
    /// As [`RefName::parse`].
    pub fn from_bytes(name: &[u8]) -> Result<Self> {
        todo!()
    }

    /// Phase 2 — validate an owned ref name without copying it.
    ///
    /// The path for a caller that already owns the bytes and would otherwise parse a borrowed
    /// slice and immediately clone it — which is every ref read, since the bytes come from a file
    /// or a `packed-refs` line that is about to be dropped.
    ///
    /// # Caveats
    ///
    /// **The name is moved in even when validation fails.** A `Result` that rejects the name gives
    /// it back inside the error, but the error carries a rendered `BString`, so an implementation
    /// that wants to hand the original back has to clone it. Callers that care about that (they
    /// should not) must use [`RefName::from_bytes`] and keep their own copy.
    ///
    /// # Errors
    ///
    /// As [`RefName::parse`].
    pub fn from_bstring(name: BString) -> Result<Self> {
        Self::from_bytes(&name)
    }

    /// Phase 2 — build the name of a local branch from its short form.
    ///
    /// # Caveats
    ///
    /// The short form is the part after `refs/heads/` — `main`, `feature/x`, `fix-1.2`. It is
    /// **not** re-validated against the abbreviated-name rules (there are none: a branch called
    /// `main` and a branch called `refs/heads/main` are, correctly, different short forms of
    /// different names).
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRefName`] if the resulting full name is not valid, which is
    /// how a short form containing `..` or a nested `.lock` is caught.
    pub fn branch(short: &BStr) -> Result<Self> {
        let full = format!("refs/heads/{}", short);
        Self::from_bytes(full.as_bytes())
    }

    /// Phase 2 — build the name of a tag from its short form.
    ///
    /// The short form is the part after `refs/tags/`.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRefName`] as [`RefName::branch`].
    pub fn tag(short: &BStr) -> Result<Self> {
        let full = format!("refs/tags/{}", short);
        Self::from_bytes(full.as_bytes())
    }

    /// Phase 2 — build a remote-tracking branch name from its two parts.
    ///
    /// `remote` is the remote's name (`origin`) and `branch` is the branch name on the remote
    /// (`main`), giving `refs/remotes/origin/main`.
    ///
    /// # Caveats
    ///
    /// * This builds the *default* name only. A remote may be configured with a different
    ///   `fetch` refspec — `+refs/heads/*:refs/remotes/upstream/*` — in which case that refspec,
    ///   not this function, decides where a fetched ref lands.
    /// * A branch on the remote may itself contain `/` (`feature/x`), giving a three-component
    ///   remote-tracking name. That is legal and common; a parser that assumed exactly one slash
    ///   after the remote name will misread it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidRefName`] as [`RefName::branch`].
    pub fn remote_tracking(remote: &BStr, branch: &BStr) -> Result<Self> {
        let full = format!("refs/remotes/{}/{}", remote, branch);
        Self::from_bytes(full.as_bytes())
    }

    /// Phase 2 — the name of the ref `HEAD` normally points at.
    ///
    /// # Caveats
    ///
    /// This constructs `HEAD`; it does not read it, and it does not ask whether `HEAD` is
    /// currently symbolic. A detached `HEAD` still has this name, which is why callers that want
    /// "the current branch" must use [`crate::refs::RefStore::head_branch`] instead.
    pub fn head() -> Self {
        Self::from_bytes(b"HEAD").expect("HEAD is a valid ref name")
    }

    /// Phase 2 — the name as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// Phase 2 — the name as a borrowed `&BStr`.
    pub fn as_bstr(&self) -> &BStr {
        self.0.as_ref()
    }

    /// Phase 2 — take the owned bytes back.
    pub fn into_bstring(self) -> BString {
        self.0
    }

    /// Phase 2 — which namespace this name lives in.
    ///
    /// # Caveats
    ///
    /// Answered from the name alone. A bare pseudo-ref (`MERGE_HEAD`, `FETCH_HEAD`) is
    /// [`RefCategory::Other`], not a branch, even though it holds a commit id — pseudo-refs are
    /// written by an operation in progress and are not refs anyone can name in a revision.
    pub fn category(&self) -> RefCategory {
        let name = self.as_bstr();
        if name == b"HEAD" {
            RefCategory::Head
        } else if name.starts_with(b"refs/heads/") {
            RefCategory::Branch
        } else if name.starts_with(b"refs/tags/") {
            RefCategory::Tag
        } else if name.starts_with(b"refs/remotes/") {
            RefCategory::Remote
        } else {
            RefCategory::Other
        }
    }

    /// Phase 2 — the part of the name after its namespace prefix.
    ///
    /// `refs/heads/feature/x` shortens to `feature/x`; `refs/tags/v1.0` to `v1.0`; a name in no
    /// known namespace shortens to itself.
    ///
    /// # Caveats
    ///
    /// **The short form is not unique.** `refs/heads/v1.0` and `refs/tags/v1.0` both shorten to
    /// `v1.0`, and Git resolves an ambiguous short name by its disambiguation order, preferring a
    /// tag. Nothing here decides between them — this is a display and lookup helper, and code that
    /// turns a short name back into a [`RefName`] must already know which namespace it means.
    pub fn short_name(&self) -> &BStr {
        match self.category() {
            RefCategory::Branch => BStr::new(&self.0[b"refs/heads/".len()..]),
            RefCategory::Tag => BStr::new(&self.0[b"refs/tags/".len()..]),
            RefCategory::Remote => BStr::new(&self.0[b"refs/remotes/".len()..]),
            RefCategory::Note => BStr::new(&self.0[b"refs/notes/".len()..]),
            RefCategory::Head | RefCategory::Other => BStr::new(&self.0[..]),
        }
    }

    /// Phase 2 — whether this is the literal `HEAD`.
    pub fn is_head(&self) -> bool {
        self.0 == b"HEAD"
    }

    /// Phase 2 — whether this name is under `refs/heads/`.
    pub fn is_branch(&self) -> bool {
        self.category() == RefCategory::Branch
    }

    /// Phase 2 — whether this name is under `refs/tags/`.
    pub fn is_tag(&self) -> bool {
        self.category() == RefCategory::Tag
    }

    /// Phase 2 — whether this name is under `refs/remotes/`.
    pub fn is_remote(&self) -> bool {
        self.category() == RefCategory::Remote
    }

    /// Phase 2 — whether this is a one-component pseudo-ref like `HEAD` or `MERGE_HEAD`.
    ///
    /// # Caveats
    ///
    /// True for `HEAD`, `FETCH_HEAD`, `ORIG_HEAD`, `MERGE_HEAD`, `CHERRY_PICK_HEAD` and anything
    /// else Git writes at the top of the git directory. Two consequences matter to callers:
    /// pseudo-refs are usually excluded from ref listings (`git branch -r`, `--all`), and some of
    /// them — `MERGE_HEAD` in particular — legitimately hold **more than one** line, so reading
    /// them as a plain object id is wrong.
    ///
    /// [`RefCategory::Head`] is a subset of this: every `Head` is a pseudo-ref, but `MERGE_HEAD`
    /// is a pseudo-ref and is not `Head`.
    pub fn is_pseudo_ref(&self) -> bool {
        !self.0.contains(&b'/')
    }

    /// Phase 2 — where this ref's file lives.
    ///
    /// `git_dir` joined with the name, so `refs/heads/main` becomes `.git/refs/heads/main`. This
    /// is also the path of the lock file's target, and the derivation of `logs/` is
    /// [`RefName::reflog_path`].
    ///
    /// # Caveats
    ///
    /// * **`git_dir` is the git directory, not the work tree**, and for a worktree-aware
    ///   repository the ref may live in the *common* directory instead — `commondir`/`gitdir`
    ///   redirection is out of scope for v1 (plan §1), so this takes the one directory it is
    ///   given and does not consult them.
    /// * **The result may be a directory's name rather than a file's.** Nothing here checks the
    ///   filesystem; a ref that does not exist and a ref that cannot exist are the same answer.
    /// * On Unix the byte string converts losslessly to an `OsStr`; on Windows a ref name with
    ///   non-UTF-8 bytes cannot be represented and the conversion is the one place the byte-exact
    ///   promise breaks. Git itself requires ref names to be ASCII on such filesystems.
    pub fn file_path(&self, git_dir: &Path) -> PathBuf {
        #[cfg(unix)]
        let os_name = <OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&self.0[..]);
        #[cfg(not(unix))]
        let os_name = OsStr::new(&String::from_utf8_lossy(&self.0[..]));
        git_dir.join(os_name)
    }

    /// Phase 2 — where this ref's reflog lives.
    ///
    /// `git_dir` joined with `logs/` and the name, so `refs/heads/main` becomes
    /// `.git/logs/refs/heads/main` and `HEAD` becomes `.git/logs/HEAD`.
    ///
    /// # Caveats
    ///
    /// * The `logs/` prefix goes on the **front of the whole name**, not inside it: it is
    ///   `logs/refs/heads/main`, never `refs/heads/logs/main`. Getting this wrong creates a
    ///   reflog that nothing reads and that `gc` will eventually expire as garbage.
    /// * A ref with no reflog is not an error and is not an empty file — it is an absent file.
    ///   Whether one exists is a policy question (`core.logAllRefUpdates`) decided by the writer,
    ///   so this returns a path whether or not anything is there.
    pub fn reflog_path(&self, git_dir: &Path) -> PathBuf {
        #[cfg(unix)]
        let os_name = <OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&self.0[..]);
        #[cfg(not(unix))]
        let os_name = OsStr::new(&String::from_utf8_lossy(&self.0[..]));
        git_dir.join("logs").join(os_name)
    }
}

impl fmt::Display for RefName {
    /// Phase 2 — the name as text, for error messages and CLI output.
    ///
    /// # Caveats
    ///
    /// The bytes are rendered with `bstr`'s lossy escaping, so a name with non-UTF-8 bytes prints
    /// with escapes rather than as replacement characters. This is for humans; code that needs the
    /// name itself uses [`RefName::as_bytes`] or [`RefName::as_bstr`] and never round-trips
    /// through this.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}
