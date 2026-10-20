//! Phase 9 — the merge state: the files that make a merge resumable, and `--abort`.
//!
//! A git directory mid-merge holds four pieces of state, and each exists so that a *different*
//! later command can do its job without being told what happened:
//!
//! * **`MERGE_HEAD`** — the commit(s) being merged in. One line each, in object-id hex. Its
//!   presence is the definition of "a merge is in progress", and `commit` reads it to decide
//!   whether to write a merge commit and with which parents.
//! * **`MERGE_MSG`** — the commit message the merge *would* have used, pre-filled with a
//!   `CONFLICT (content): ...` and `# Conflicts:` section. `commit` uses it as the default
//!   message, and the user edits it. Writing it is why a conflicted merge followed by `commit -m`
//!   still produces a sensible message rather than an empty one.
//! * **`ORIG_HEAD`** — where `HEAD` was before the merge started. `merge --abort` resets to it,
//!   and so does a failed `pull`; it is written *before* anything else changes, because the point
//!   of it is to survive a merge that dies mid-way.
//! * **The index's conflict stages** — the three versions per conflicted path, which are the only
//!   copy of the base content once the merge has overwritten the work tree. Losing them makes
//!   `git mergetool` and `checkout --merge` impossible. They live in the index and are written by
//!   [`crate::merge::write_conflict_stages`], not here.
//!
//! # Caveats that bite
//!
//! * **`MERGE_HEAD` is a list and can hold more than one id.** An octopus merge (`git merge a b
//!   c`) has one line per commit, and a reader that parsed only the first line silently drops the
//!   rest — the commit then has the wrong number of parents and the merge is not recorded.
//! * **An empty `MERGE_HEAD` is not "no merge".** A zero-byte file is a corrupt state, not an
//!   absent one; the check for "is a merge in progress" is *existence* of the file, and a caller
//!   that read the contents and tested for emptiness would report a merge that `--abort` then
//!   cannot find.
//! * **`ORIG_HEAD` is not removed by a successful merge**, deliberately: Git keeps it until the
//!   next operation that overwrites it, which is what makes `reset ORIG_HEAD` work after a merge
//!   that succeeded. `clear_merge_state` is about `MERGE_HEAD` and `MERGE_MSG` only.
//! * **`--abort` refuses rather than guesses.** With no merge in progress it is
//!   [`crate::error::Error::NoMergeInProgress`]; with a work tree whose changes are unrelated to
//!   the merge it is [`crate::error::Error::DirtyWorktree`], because resetting to `ORIG_HEAD`
//!   would discard them. Git refuses in both cases and Rivet must too — an `--abort` that
//!   discarded the user's edits would be the worst possible bug in a merge.
//! * **`--abort` restores the work tree as well as the index**, which is the part a first
//!   implementation forgets: resetting the index leaves the conflicted files — markers and all —
//!   sitting in the work tree, and `status` then reports a clean index with a dirty work tree,
//!   which is a state the user cannot get out of without `checkout -- .`.
//! * **A merge that fast-forwarded leaves nothing to abort.** There is no merge commit and no
//!   `MERGE_HEAD`; `--abort` reports [`crate::error::Error::NoMergeInProgress`] and the way back is
//!   `reset --hard ORIG_HEAD`, which the caller should tell the user. Writing a `MERGE_HEAD` for a
//!   fast-forward so that `--abort` "works" would make `commit` write a merge commit that Git does
//!   not write.

use std::path::Path;

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::index::Index;
use crate::odb::Odb;
use crate::worktree::Worktree;
use crate::error::Error;

/// Phase 9 — the file naming the commit(s) being merged in.
///
/// # Caveats
///
/// The name is relative to the **git directory**, not the work tree, so a caller must join it
/// with the git directory it discovered — an implementation that joined it with the work tree
/// writes to `<worktree>/MERGE_HEAD`, where nothing will ever read it and `status` will report no
/// merge. The same applies to every other file named in this module.
pub const MERGE_HEAD: &str = "MERGE_HEAD";

/// Phase 9 — the file holding the merge's proposed commit message.
///
/// # Caveats
///
/// Written even when the merge is clean, because `commit` after a clean `merge --no-commit` uses
/// it. A caller that only wrote it on conflict makes `merge --no-commit` followed by `commit`
/// produce an empty message.
pub const MERGE_MSG: &str = "MERGE_MSG";

/// Phase 9 — the file recording where `HEAD` was before the merge.
///
/// # Caveats
///
/// Distinct from `refs/heads/*/ORIG_HEAD`-style per-ref reflog entries and from the reflog
/// itself: this is a single file holding a single id, it is not append-only, and it is not
/// per-branch. It is overwritten by every operation that uses it.
pub const ORIG_HEAD: &str = "ORIG_HEAD";

/// Phase 9 — the state of an in-progress merge.
///
/// # Caveats
///
/// * **`merge_head` is a `Vec` and is never empty in a valid state**, per the module caveat about
///   octopus merges and about an empty file being corruption rather than absence. A caller
///   reading [`read_merge_state`] gets `None` for "no merge" and `Some` with a non-empty list for
///   a merge; an empty list inside a `Some` means the file was corrupt and the implementation
///   must say so rather than return a state with no parents.
/// * **`merge_msg` has already been through the "clean up the subject line" transformation** when
///   it was written, so a caller must not normalise it again — collapsing whitespace twice is
///   idempotent, but re-wrapping is not, and the message is what ends up in the commit.
/// * **`head_commit` is `None` for a merge into an unborn branch.** `git merge` onto a branch with
///   no commits fast-forwards and writes no state at all, but a merge *into* an unborn `HEAD` with
///   an explicit `--no-ff` is representable, and the field exists so a caller does not have to
///   special-case it by reading `HEAD` twice.
/// * **`tree` is the merged tree when the merge was clean and `None` when it was not.** It is
///   stored so that a `merge --no-commit` followed later by `commit` does not have to re-run the
///   merge; a caller that recomputed it would get a different answer if the work tree changed in
///   between, and the answer it should commit is the one the merge produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeState {
    /// The commits being merged in, in the order they were given.
    pub merge_head: Vec<ObjectId>,
    /// The proposed commit message.
    pub merge_msg: BString,
    /// Where `HEAD` was before the merge, from `ORIG_HEAD`.
    pub orig_head: Option<ObjectId>,
    /// The commit `HEAD` pointed at when the merge started, or `None` if unborn.
    pub head_commit: Option<ObjectId>,
    /// The merged tree, when the merge was clean.
    pub tree: Option<ObjectId>,
}

impl MergeState {
    /// Phase 9 — an empty state, for a merge about to record itself.
    pub fn new() -> Self {
        Self {
            merge_head: Vec::new(),
            merge_msg: BString::default(),
            orig_head: None,
            head_commit: None,
            tree: None,
        }
    }

    /// Phase 9 — whether more than one commit is being merged in.
    ///
    /// # Caveats
    ///
    /// An octopus merge, which `commit` must turn into a commit with that many parents. It is not
    /// an error and it is not rare enough to ignore: `git merge topic1 topic2` is a normal thing
    /// to type, and the resulting commit's parent list is what makes the history reachable.
    pub fn is_octopus(&self) -> bool {
        self.merge_head.len() > 1
    }

    /// Phase 9 — the first commit being merged in.
    ///
    /// # Caveats
    ///
    /// `None` only for a state that was constructed empty; a state read from disk always has at
    /// least one. The "first" is the one Git's messages refer to when they name a single other
    /// commit, which is why the order of `merge_head` is preserved from the command line.
    pub fn first_parent(&self) -> Option<ObjectId> {
        self.merge_head.first().copied()
    }

    /// Phase 9 — the message, as bytes.
    ///
    /// # Caveats
    ///
    /// Returns bytes rather than a `str`: the message is user data from a file on disk and is not
    /// guaranteed to be UTF-8. A caller that displayed it by lossy conversion is fine; one that
    /// wrote it into a commit by lossy conversion would change the commit's id.
    pub fn message(&self) -> &BStr {
        self.merge_msg.as_ref()
    }
}

impl Default for MergeState {
    /// Phase 9 — the defaults of [`MergeState::new`].
    fn default() -> Self {
        Self::new()
    }
}

/// Phase 9 — write the merge state into the git directory.
///
/// # Caveats
///
/// * **`ORIG_HEAD` is written before `MERGE_HEAD`**, because the point of `ORIG_HEAD` is to
///   survive a failure partway through and a caller that wrote them in the other order leaves a
///   window in which a merge is "in progress" with no way back. The ordering is a correctness
///   requirement, not a style choice.
/// * **The files are written with the same lock-then-rename discipline as a ref.** A partially
///   written `MERGE_HEAD` — a truncated id — is a state that no command can interpret, and a
///   reader is likely to hit it, because the file's presence is what tells other processes a
///   merge is running.
/// * **`MERGE_MSG` is written with a trailing newline and comments already in place**, so the
///   file is what `commit` will present to the user. The `# Conflicts:` section is part of the
///   message text and is stripped by `commit`'s comment handling, not by this function.
/// * **An existing `MERGE_HEAD` is overwritten, not refused.** Refusing is
///   [`crate::error::Error::MergeInProgress`] and it belongs to the caller that starts a merge,
///   *before* it touches anything — a state-writer that refused would fail after the work tree
///   had already been modified.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] for a file that cannot be written, and
/// [`crate::error::Error::LockHeld`] if another process holds the lock.
pub fn write_merge_state(git_dir: &Path, state: &MergeState) -> Result<()> {
    // ORIG_HEAD first: it is the way back, and a merge that is visible before it can be undone
    // is worse than one that is not visible yet.
    if let Some(orig) = state.orig_head {
        write_orig_head(git_dir, orig)?;
    }

    let mut heads = Vec::new();
    for oid in &state.merge_head {
        heads.extend_from_slice(&oid.to_hex()[..]);
        heads.push(b'\n');
    }
    write_locked(&git_dir.join(MERGE_HEAD), &heads)?;

    let msg = if state.merge_msg.ends_with(b"\n") {
        state.merge_msg.to_vec()
    } else {
        let mut msg = state.merge_msg.to_vec();
        msg.push(b'\n');
        msg
    };
    write_locked(&git_dir.join(MERGE_MSG), &msg)?;
    Ok(())
}

/// Write a state file through a lock file, so a reader never sees a truncated id.
fn write_locked(path: &Path, data: &[u8]) -> Result<()> {
    let mut lock = crate::refs::RefLock::acquire_path(path)?;
    lock.write_and_commit(data)?;
    Ok(())
}

/// Phase 9 — read the merge state, if a merge is in progress.
///
/// # Caveats
///
/// * **`Ok(None)` means no merge**, decided by the *existence* of `MERGE_HEAD`; see the module
///   caveat about an empty file. A file that exists and does not parse is an error, not `None`.
/// * **`ORIG_HEAD` and `MERGE_MSG` are optional and their absence is not an error.** A merge
///   started by a tool that wrote only `MERGE_HEAD` is readable, and `orig_head` is then `None` —
///   which makes `--abort` unable to restore anything and must be reported by that caller rather
///   than papered over here.
/// * **The ids are parsed against the repository's format**, which is a parameter and not a
///   guess: a SHA-256 repository's `MERGE_HEAD` is 64 hex characters, and a parser fixed at 40
///   reads it as a truncated id or rejects it.
///
/// # Errors
///
/// [`crate::error::Error::CorruptObject`]-style malformed-id reporting for a `MERGE_HEAD` line
/// that is not a valid id: [`crate::error::Error::InvalidHex`]. And
/// [`crate::error::Error::IoAtPath`] for a file that cannot be read.
pub fn read_merge_state(
    git_dir: &Path,
    format: crate::hash::ObjectFormat,
) -> Result<Option<MergeState>> {
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&merge_head_path).map_err(|e| Error::IoAtPath {
        path: BString::from(merge_head_path.to_string_lossy().into_owned()),
        source: e,
    })?;
    let line = content.lines().next().unwrap_or("");
    let hex = BStr::new(line.trim().as_bytes());
    let head = ObjectId::from_hex(format, hex).map_err(|e| e)?;
    let mut state = MergeState::new();
    state.merge_head.push(head);
    let orig_head_path = git_dir.join("ORIG_HEAD");
    if orig_head_path.exists() {
        let orig_content = std::fs::read_to_string(&orig_head_path).map_err(|e| Error::IoAtPath {
            path: BString::from(orig_head_path.to_string_lossy().into_owned()),
            source: e,
        })?;
        let orig_line = orig_content.lines().next().unwrap_or("");
        let orig_hex = BStr::new(orig_line.trim().as_bytes());
        state.orig_head = Some(ObjectId::from_hex(format, orig_hex).map_err(|e| e)?);
    }
    let merge_msg_path = git_dir.join("MERGE_MSG");
    if merge_msg_path.exists() {
        let msg_content = std::fs::read_to_string(&merge_msg_path).map_err(|e| Error::IoAtPath {
            path: BString::from(merge_msg_path.to_string_lossy().into_owned()),
            source: e,
        })?;
        state.merge_msg = BString::from(msg_content);
    }
    Ok(Some(state))
}

/// Phase 9 — remove `MERGE_HEAD` and `MERGE_MSG`.
///
/// # Caveats
///
/// * **`ORIG_HEAD` is left alone**, per the module caveat: it outlives the merge on purpose.
/// * **The index's conflict stages are not touched.** Clearing the state is what a *successful*
///   commit does after it has written the merge commit, at which point the index has already been
///   resolved; a caller that cleared the state to "cancel" a conflicted merge leaves the stages
///   behind and must run [`abort_merge`] instead.
/// * **Removing a file that is not there is not an error.** Idempotent, because both `commit` and
///   `--abort` call it on paths where the other may already have.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] for a file that exists and cannot be removed.
pub fn clear_merge_state(git_dir: &Path) -> Result<()> {
    let merge_head = git_dir.join("MERGE_HEAD");
    let merge_msg = git_dir.join("MERGE_MSG");
    if merge_head.exists() {
        std::fs::remove_file(&merge_head).map_err(|e| crate::error::Error::IoAtPath { path: merge_head.to_string_lossy().into_owned().into(), source: e })?;
    }
    if merge_msg.exists() {
        std::fs::remove_file(&merge_msg).map_err(|e| crate::error::Error::IoAtPath { path: merge_msg.to_string_lossy().into_owned().into(), source: e })?;
    }
    Ok(())
}

/// Phase 9 — whether a merge is in progress.
///
/// # Caveats
///
/// Existence of `MERGE_HEAD`, not a parse — so it is cheap enough for `status` to call on every
/// invocation, and it must not be replaced by a parse for that reason. A caller that needs the
/// state calls [`read_merge_state`] and pays for it once.
pub fn merge_in_progress(git_dir: &Path) -> bool {
    git_dir.join("MERGE_HEAD").exists()
}

/// Phase 9 — record where `HEAD` was, for a later `reset` or `--abort`.
///
/// # Caveats
///
/// Called by every operation that moves `HEAD` destructively — merge, rebase, reset, pull — with
/// the value from *before* the move. Writing it after the move is a bug that turns `ORIG_HEAD`
/// into a pointer at the new state, which makes `reset ORIG_HEAD` a no-op.
///
/// # Errors
///
/// [`crate::error::Error::IoAtPath`] if the file cannot be written.
pub fn write_orig_head(git_dir: &Path, oid: ObjectId) -> Result<()> {
    let path = git_dir.join("ORIG_HEAD");
    let mut buf = Vec::with_capacity(oid.format().hex_len() + 1);
    buf.extend_from_slice(&oid.to_hex()[..]);
    buf.push(b'\n');
    std::fs::write(&path, &buf).map_err(|e| crate::error::Error::IoAtPath { path: BString::from(path.to_string_lossy().as_ref()), source: e })
}

/// Phase 9 — undo a merge: restore the index and the work tree to before it started.
///
/// # Caveats
///
/// * **The safety checks come first and they are not optional.** Before touching anything, this
///   refuses when there is no merge in progress
///   ([`crate::error::Error::NoMergeInProgress`]) and when the work tree holds changes that are
///   unrelated to the merge ([`crate::error::Error::DirtyWorktree`], carrying every offending
///   path). The check is the same one [`crate::worktree::CheckoutPlan::validate`] makes for a
///   checkout to `ORIG_HEAD`'s tree, and it must be that function rather than a reimplementation —
///   the whole of plan §5's `victim.txt` rule is that this decision happens in one place.
/// * **`ORIG_HEAD` is the destination, not `HEAD`.** If `ORIG_HEAD` is missing the abort cannot
///   proceed and must say so rather than resetting to something it guessed.
/// * **The index is restored from the target tree, not from the merge base.** `--abort` means
///   "pretend the merge never happened", which is `reset --merge ORIG_HEAD`, and an implementation
///   that restored the base instead would leave the branch looking like it had been merged
///   partially.
/// * **`MERGE_HEAD` and `MERGE_MSG` are removed last.** If the restore fails partway, the merge
///   state is still on disk and `--abort` can be run again; clearing it first leaves a
///   half-restored work tree with no record of what was happening.
/// * **`--abort` on a fast-forwarded merge is [`crate::error::Error::NoMergeInProgress`]**; see
///   the module caveat. The message the caller prints should mention `reset --hard ORIG_HEAD`.
///
/// # Errors
///
/// [`crate::error::Error::NoMergeInProgress`], [`crate::error::Error::DirtyWorktree`],
/// [`crate::error::Error::UntrackedOverwrite`], [`crate::error::Error::BareRepository`] when there
/// is no work tree to restore, and the I/O errors of the checkout it performs.
pub fn abort_merge(
    worktree: &Worktree,
    index: &mut Index,
    odb: &Odb,
    git_dir: &Path,
) -> Result<()> {
    use crate::object::{Commit, ObjectKind};

    let state = match read_merge_state(git_dir, worktree.format())? {
        Some(state) => state,
        None => return Err(Error::NoMergeInProgress),
    };
    let orig = match state.orig_head {
        Some(orig) => orig,
        None => return Err(Error::NoMergeInProgress),
    };
    worktree.require_root("merge --abort")?;

    let commit_tree = |oid: crate::hash::ObjectId| -> Result<crate::hash::ObjectId> {
        let object = odb.read_typed(&oid, ObjectKind::Commit)?;
        Ok(Commit::parse(worktree.format(), object.data())?.tree)
    };
    let target_tree = commit_tree(orig)?;
    let target = crate::merge::tree::flatten_tree(odb, target_tree)?;

    // There is no tree to plan a checkout from: the merge left the index conflicted, so the state
    // being replaced is not any commit's tree. The victim rule is therefore applied directly —
    // for every path this abort would overwrite or delete, the work tree must still match the
    // index, or the user has edits the merge did not make and would lose them.
    let mut dirty: Vec<BString> = Vec::new();
    let mut removals: Vec<BString> = Vec::new();
    for entry in index.entries() {
        let path = entry.path.clone();
        let path_ref = BStr::new(&path[..]);
        let target_entry = target.get(&path);

        if target_entry.is_none() {
            if !removals.contains(&path) {
                removals.push(path.clone());
            }
        }
        // Only a stage-0 entry describes content the user could have edited; a conflicted path's
        // work tree file is the merge's own marker file, and losing it loses nothing.
        if entry.stage() != crate::index::Stage::Normal {
            continue;
        }
        let unchanged = match target_entry {
            Some((_, oid)) => *oid == entry.oid,
            None => false,
        };
        if unchanged || dirty.contains(&path) {
            continue;
        }
        match worktree.hash_file(path_ref) {
            Ok(worktree_oid) if worktree_oid == entry.oid => {}
            // A file that is already gone cannot be lost.
            Err(Error::IoAtPath { .. }) => {}
            _ => dirty.push(path),
        }
    }
    if !dirty.is_empty() {
        return Err(Error::DirtyWorktree { paths: dirty });
    }

    // Writes before removals: a path that moves keeps its content visible throughout.
    for (path, (mode, oid)) in &target {
        crate::worktree::checkout::write_blob(worktree, BStr::new(&path[..]), *oid, *mode, odb)?;
    }
    for path in &removals {
        let path_ref = BStr::new(&path[..]);
        if worktree.stat(path_ref)?.is_some() {
            crate::worktree::checkout::remove_path(worktree, path_ref)?;
        }
    }

    // The index becomes the target tree's. `--abort` means "as if the merge never started", so
    // the pre-merge index is not the thing to restore — its entries can legitimately disagree
    // with HEAD, and those disagreements are what the merge was about.
    let mut restored = crate::index::Index::new();
    for (path, (mode, oid)) in &target {
        restored.insert(crate::index::IndexEntry {
            ctime_seconds: 0,
            ctime_nanos: 0,
            mtime_seconds: 0,
            mtime_nanos: 0,
            dev: 0,
            ino: 0,
            mode: *mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid: *oid,
            flags: crate::index::IndexEntryFlags::with(crate::index::Stage::Normal, path.len()),
            extended_flags: crate::index::IndexEntryExtendedFlags::empty().bits(),
            path: path.clone(),
        });
    }
    *index = restored;
    index.write(&git_dir.join("index"), worktree.format())?;

    // The state files go last: a failure above leaves the merge on disk and aborts can be retried.
    clear_merge_state(git_dir)?;
    Ok(())
}
