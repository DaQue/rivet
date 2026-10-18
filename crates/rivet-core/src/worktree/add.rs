//! Phase 3 — `add` and `rm`: turning work tree files into index entries, and taking them out.
//!
//! The index is a flat, sorted list of `(path, stage)` entries whose stat fields are a cache of
//! the file they describe. `add` is therefore three things at once, and a caller that does only
//! the first produces an index that works and is slow forever after:
//!
//! 1. **Hash the file** into a blob and write it to the object database, keyed by content.
//! 2. **Build an entry** with the id, the normalised mode and the `lstat` data.
//! 3. **Replace the entry for that path and stage**, and keep the list sorted — which, because
//!    Git binary-searches the index, is a correctness requirement rather than tidiness.
//!
//! # Caveats that bite
//!
//! * **The mode is normalised, never stored as found.** `100664` becomes `100644`, a sticky or
//!   setgid bit is dropped, and with `core.filemode = false` the executable bit is ignored
//!   entirely. Storing a mode Git would not write produces an index that differs from Git's for
//!   identical content, which is the failure the differential tests exist to catch.
//! * **`add` on a conflicted path resolves it.** Git's `add` during a merge is the *resolution*
//!   operation: it drops every stage for the path and writes a stage-0 entry. That is
//!   [`crate::index::IndexExt::resolve`], and it is why a `merge` that leaves conflicts is
//!   finished with `add` rather than with a special command.
//! * **A directory argument means "everything under it", filtered by ignore rules.** `add .`
//!   does not stage an ignored file, and it does not stage a file that is already current — but
//!   it *does* stage a deletion under the directory, which is why the walk and the index must
//!   both be consulted.
//! * **An ignored path is refused when named explicitly and skipped when found by a walk.**
//!   `git add build/out.o` fails with [`crate::error::Error::PathIgnored`]; `git add .` skips
//!   it silently. The asymmetry is deliberate in Git and the `-f` flag is the override.
//! * **`rm` stages a deletion and, by default, removes the file.** `--cached` keeps it. Either
//!   way the work tree is only touched after the index has been decided, so an `rm` that fails
//!   halfway does not leave a file gone from the index and present on disk.
//! * **An empty directory is not addable.** Git tracks files; `add` on a directory with no
//!   tracked and no trackable content is a no-op, not an error — Git warns and moves on, and so
//!   must Rivet, or a script adding a build directory that happens to be empty would fail.
//! * **`add -A` spans the whole work tree and can be very large.** There is no incremental form
//!   and no signature that would make one possible: the deletion half of the operation is
//!   discovered by comparing the index against the work tree, which is a full comparison.
//! * **Nothing here writes the index file.** Every function mutates an [`Index`] in memory; the
//!   caller writes it through [`crate::index::Index::write`], which takes the `index.lock` and
//!   is the only atomic step in the sequence. A caller that forgets to write has staged nothing.

use bstr::{BStr, BString};

use crate::error::Result;
use crate::hash::ObjectFormat;
use crate::ignore::IgnoreStack;
use crate::index::{Index, IndexEntry, Stage};
use crate::object::EntryMode;
use crate::odb::Odb;
use crate::worktree::{StatCache, Worktree};

/// Phase 3 — how an `add` should behave.
///
/// # Caveats
///
/// * **`force` is the only way past an ignore rule.** It is `git add -f`, and it applies to a
///   path named explicitly as well as one found by a walk — the two differ in whether an
///   ignored path is an error, not in whether the rule can be overridden.
/// * **`update` and `all` are not the same flag and the difference is the deletion half.**
///   `update` stages modifications and deletions of *tracked* paths and never adds a new file;
///   `all` does both. Collapsing them makes `add -u` stage files the user never mentioned.
/// * **`dry_run` still hashes files.** There is no way to know whether a file's content is
///   current without reading it, so a dry run costs almost what a real one does — which is why
///   Git's own `add -n` is documented as slow.
/// * **`intent_to_add` writes a stage-0 entry pointing at the empty blob** without storing the
///   content, which is what `git add -N` does; the file then shows as a change in `diff` and as
///   an addition in `status`. It is a state the index can hold and the commit cannot: a commit
///   with an intent-to-add entry must fail with [`crate::error::Error::UnmergedPath`]-style
///   refusal rather than recording the empty blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddOptions {
    /// Stage ignored paths anyway.
    pub force: bool,
    /// Stage only paths the index already knows about.
    pub update: bool,
    /// Stage modifications, deletions and new files under the given paths.
    pub all: bool,
    /// Record intent-to-add entries instead of storing content.
    pub intent_to_add: bool,
    /// Do everything but write the index.
    pub dry_run: bool,
    /// A path prefix to limit the operation to, empty for the paths given.
    pub pathspec: BString,
}

impl AddOptions {
    /// Phase 3 — the options a plain `rivet add <paths>` uses: no force, no update, no all, no
    /// intent-to-add, no dry run, no extra pathspec.
    pub fn new() -> Self {
        Self {
            force: false,
            update: false,
            all: false,
            intent_to_add: false,
            dry_run: false,
            pathspec: BString::from(""),
        }
    }

    /// Phase 3 — set the force flag, which overrides ignore rules.
    pub fn force(&mut self, on: bool) -> &mut Self {
        todo!()
    }

    /// Phase 3 — stage only paths already in the index.
    pub fn update(&mut self, on: bool) -> &mut Self {
        todo!()
    }

    /// Phase 3 — record intent-to-add entries.
    pub fn intent_to_add(&mut self, on: bool) -> &mut Self {
        todo!()
    }
}

/// Phase 3 — what an `add` did.
///
/// # Caveats
///
/// * **The counts are of index entries, not of files.** A path that had conflict stages and is
///   now resolved accounts for up to three removed entries and one added one.
/// * **`stored_ids` is not the whole story of what was written.** A file added twice with the
///   same content writes one object the first time and nothing the second, because
///   [`crate::odb::Odb::write`] is idempotent — so the object database's size is not this list's
///   length, and a caller counting written objects has to ask the database.
/// * **An empty outcome is success.** `add` on a path that is already current changes nothing
///   and reports nothing, which is what makes `add .` idempotent and safe to run repeatedly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddOutcome {
    /// How many paths got a new or replaced stage-0 entry.
    pub added: usize,
    /// How many entries were updated because their content or mode had changed.
    pub updated: usize,
    /// How many entries, all stages included, were removed because the file is gone.
    pub removed: usize,
    /// The paths that were touched, sorted — what `git add -v` prints.
    pub paths: Vec<BString>,
    /// The object ids that were written or would have been written in a dry run.
    pub stored_ids: Vec<crate::hash::ObjectId>,
}

impl AddOutcome {
    /// Phase 3 — whether the operation changed nothing.
    pub fn is_empty(&self) -> bool {
        todo!()
    }
}

/// Phase 3 — how an `rm` should behave.
///
/// # Caveats
///
/// * **`cached` is the difference between "stop tracking" and "delete".** With it, the file
///   stays on disk, becomes untracked, and the next `status` reports it as such — which is
///   exactly what a user who forgot to add a `.gitignore` entry wants.
/// * **`recursive` is required to remove a directory.** Git refuses `rm dir` without it, because
///   the alternative is a command that deletes an unbounded number of files when the user meant
///   one; the refusal is `Error::Message` in Git and the same shape here.
/// * **`force` is about the *content*, not the file.** Git refuses to remove a path whose work
///   tree content differs from the index — the same safety instinct as checkout — and `force`
///   is the override.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RmOptions {
    /// Remove from the index but leave the file in the work tree.
    pub cached: bool,
    /// Allow a directory to be removed.
    pub recursive: bool,
    /// Remove even when the file has unstaged changes.
    pub force: bool,
    /// Do everything but write anything.
    pub dry_run: bool,
}

impl RmOptions {
    /// Phase 3 — the options a plain `rivet rm <paths>` uses: remove from the index *and* the
    /// work tree, no recursion, no force, no dry run.
    pub fn new() -> Self {
        todo!()
    }

    /// Phase 3 — keep the file in the work tree.
    pub fn cached(&mut self, on: bool) -> &mut Self {
        todo!()
    }

    /// Phase 3 — allow removing a directory.
    pub fn recursive(&mut self, on: bool) -> &mut Self {
        todo!()
    }
}

/// Phase 3 — stage the given paths.
///
/// # Caveats
///
/// * **The index is mutated in memory and not written.** See the module caveat; the caller owns
///   the lock and the atomic write.
/// * **The index must be sorted afterwards.** Entries are inserted into a sorted list here, but
///   an implementation that appends must call [`crate::index::Index::sort`] before writing, or
///   the file is one Git cannot binary-search.
/// * **Paths that do not exist are deletions when the index knows them and errors when it does
///   not.** `add missing.c` with no index entry is [`crate::error::Error::PathNotFound`];
///   `add deleted.c` for a tracked file stages its removal.
/// * **An ignored path named explicitly is [`crate::error::Error::PathIgnored`]**, carrying the
///   pattern's origin so the message can say which rule and which line — which is the whole
///   value of `check-ignore -v`.
/// * **A submodule's directory is a gitlink.** Descending into it would stage the *other*
///   repository's files; the entry is preserved as it is and the directory is not walked.
///
/// # Errors
///
/// [`crate::error::Error::PathNotFound`], [`crate::error::Error::PathIgnored`],
/// [`crate::error::Error::OutsideWorktree`] for a path prefix that is not under the root,
/// [`crate::error::Error::BareRepository`] when there is no work tree,
/// [`crate::error::Error::IoAtPath`] for a file that cannot be read, and the object database's
/// write errors.
pub fn add_paths(
    index: &mut Index,
    worktree: &Worktree,
    odb: &Odb,
    ignore: &IgnoreStack,
    stat_cache: &mut StatCache,
    paths: &[BString],
    options: &AddOptions,
) -> Result<AddOutcome> {
    todo!()
}

/// Phase 3 — unstage the given paths, and by default delete them.
///
/// # Caveats
///
/// * **Every stage for a path is removed**, not just stage 0. An `rm` on a conflicted path is
///   the user saying "I do not want this at all", and leaving stages 1–3 behind would make the
///   path stay conflicted with no stage-0 entry — a state `commit` refuses and `status` cannot
///   explain.
/// * **The work tree is touched after the index, never before.** A failure partway through the
///   unlinks leaves an index whose entries are gone and files that still exist, which is
///   recoverable — the reverse is not.
/// * **A `--cached` removal of a directory removes only the index entries.** The files stay,
///   which is the standard way to stop tracking a build output tree without deleting it.
///
/// # Errors
///
/// [`crate::error::Error::PathNotFound`] for a path that is in neither the index nor the work
/// tree; [`crate::error::Error::Message`] for a directory without `recursive`;
/// [`crate::error::Error::IoAtPath`] if a file cannot be removed; and
/// [`crate::error::Error::BareRepository`] for a non-`--cached` removal with no work tree.
pub fn rm_paths(
    index: &mut Index,
    worktree: &Worktree,
    paths: &[BString],
    options: &RmOptions,
) -> Result<AddOutcome> {
    todo!()
}

/// Phase 3 — build the index entry for one work tree path.
///
/// The step between hashing a file and staging it, exposed because `status`'s refresh, a
/// checkout's post-write index update and `add` all need exactly this and a second
/// implementation would be a second set of mode rules.
///
/// # Caveats
///
/// * **The stat data must have been read from `lstat`**, for the reason in
///   [`crate::worktree::stat`]: a symlink's entry describes the link, not its target.
/// * **The mode comes from `stat.mode`**, already classified through `core.filemode` and
///   `core.symlinks`; passing a raw filesystem mode here reintroduces the platform dependence
///   the classification exists to remove.
/// * **`stage` is almost always [`Stage::Normal`].** The other three are how a merge records a
///   conflict, and an `add` that wrote a stage-1 entry would leave the path unmerged forever.
/// * **The path is not validated here.** It came from an index or from a walk, and re-checking
///   it on every entry of a large `add` is a cost with no benefit; the validation belongs where
///   the path entered the program.
/// * **Nothing is written to the object database.** The id is taken as given, so a caller that
///   passes an id it did not store has staged content that does not exist — and the failure
///   surfaces much later, at commit or at checkout.
pub fn build_index_entry(
    format: ObjectFormat,
    path: &BStr,
    oid: crate::hash::ObjectId,
    stat: &crate::worktree::StatData,
    mode: EntryMode,
    stage: Stage,
) -> IndexEntry {
    todo!()
}
