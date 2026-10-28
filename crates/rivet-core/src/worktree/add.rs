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

use std::collections::BTreeSet;

use bstr::{BStr, BString};

use crate::error::{Error, Result};
use crate::hash::ObjectFormat;
use crate::ignore::{IgnoreDecision, IgnoreStack, PathKind};
use crate::index::{Index, IndexEntry, IndexEntryFlags, IndexExt, Stage};
use crate::object::{EntryMode, ObjectKind, hash_object};
use crate::odb::Odb;
use crate::worktree::{StatCache, Worktree, WorktreeWalkOptions, check_relative_path};

/// Whether the blob the index holds for `path` contains a CR — what `text=auto` asks before it
/// normalises line endings (git's `has_crlf_in_index`).
pub(crate) fn index_blob_has_cr(index: &Index, odb: &Odb, path: &BStr) -> bool {
    let Some(entry) = index.entry(path) else {
        return false;
    };
    if entry.stage() != Stage::Normal || matches!(entry.mode, EntryMode::Link | EntryMode::Commit) {
        return false;
    }
    odb.read_typed(&entry.oid, ObjectKind::Blob)
        .map(|object| object.data().contains(&b'\r'))
        .unwrap_or(false)
}

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
    /// Files found on the way that are left alone (neither hashed nor staged): the untracked files
    /// outside a sparse-checkout definition.
    pub skip_paths: Vec<BString>,
}

impl Default for AddOptions {
    fn default() -> Self {
        Self::new()
    }
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
            skip_paths: Vec::new(),
        }
    }

    /// Phase 3 — set the force flag, which overrides ignore rules.
    pub fn force(&mut self, on: bool) -> &mut Self {
        self.force = on;
        self
    }

    /// Phase 3 — stage only paths already in the index.
    pub fn update(&mut self, on: bool) -> &mut Self {
        self.update = on;
        self
    }

    /// Phase 3 — record intent-to-add entries.
    pub fn intent_to_add(&mut self, on: bool) -> &mut Self {
        self.intent_to_add = on;
        self
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
        self.added == 0 && self.updated == 0 && self.removed == 0
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

impl Default for RmOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl RmOptions {
    /// Phase 3 — the options a plain `rivet rm <paths>` uses: remove from the index *and* the
    /// work tree, no recursion, no force, no dry run.
    pub fn new() -> Self {
        Self {
            cached: false,
            recursive: false,
            force: false,
            dry_run: false,
        }
    }

    /// Phase 3 — keep the file in the work tree.
    pub fn cached(&mut self, on: bool) -> &mut Self {
        self.cached = on;
        self
    }

    /// Phase 3 — allow removing a directory.
    pub fn recursive(&mut self, on: bool) -> &mut Self {
        self.recursive = on;
        self
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
    // ── the paths, normalised and classified ────────────────────────────────────────────────
    // `specs` are the limits of the operation: they bound both what is staged and which
    // deletions are in scope. An empty spec means "the whole work tree" and is what `-A`/`-u`
    // with no paths produces.
    let mut specs: Vec<BString> = Vec::new();
    for p in paths {
        specs.push(normalize_spec(BStr::new(p))?);
    }
    if !options.pathspec.is_empty() {
        specs.push(normalize_spec(BStr::new(&options.pathspec))?);
    }
    if specs.is_empty() {
        if options.all || options.update {
            specs.push(BString::default());
        } else {
            // `add` with nothing to add is a no-op here; the CLI is where "Nothing specified"
            // belongs, because a library caller may legitimately stage an empty set.
            return Ok(AddOutcome::default());
        }
    }

    let mut outcome = AddOutcome::default();
    if !options.dry_run {
        worktree.require_root("add")?;
    }

    let empty_blob = hash_object(worktree.format(), ObjectKind::Blob, b"")?.0;

    // Files named directly, as opposed to found by walking a directory: the two differ in how an
    // ignored path is treated, and in whether a missing path is an error.
    let mut named: BTreeSet<BString> = BTreeSet::new();
    let mut files: BTreeSet<BString> = BTreeSet::new();
    let mut dirs: Vec<BString> = Vec::new();
    let mut delete_scope: Vec<BString> = Vec::new();

    for spec in &specs {
        if spec.is_empty() {
            dirs.push(spec.clone());
            delete_scope.push(spec.clone());
            continue;
        }
        let stat = stat_cache.stat(worktree, BStr::new(spec))?.cloned();
        match stat {
            Some(sd) if sd.mode == EntryMode::Tree => {
                // A gitlink is a directory that belongs to another repository. Walking it would
                // stage that repository's files under this one's paths, so the existing entry is
                // left exactly as it is.
                if index.entry(BStr::new(spec)).is_some_and(|e| e.is_gitlink())
                    || is_repository_dir(worktree, BStr::new(spec))
                {
                    continue;
                }
                dirs.push(spec.clone());
                delete_scope.push(spec.clone());
            }
            Some(_) => {
                named.insert(spec.clone());
                files.insert(spec.clone());
            }
            None => {
                // Missing on disk: either a tracked path whose deletion is being staged, or a
                // path Git never heard of. A directory that is gone entirely has no stat either,
                // so the children are consulted before declaring it unknown. Conflict stages
                // count: `add` of a deleted unmerged path stages its removal.
                let tracked_children = index.entries().iter().any(|e| is_under(&e.path, spec));
                if !tracked_children && !options.update {
                    return Err(Error::PathNotFound(spec.clone()));
                }
                delete_scope.push(spec.clone());
            }
        }
    }

    // ── directories: everything under them, filtered by the ignore stack ────────────────────
    for dir in &dirs {
        let mut walk_options = WorktreeWalkOptions::new();
        walk_options.prefix = dir.clone();
        walk_options.recursive = true;
        // `-f` is the only way an ignored file is staged, and it applies to a walk exactly as it
        // applies to a named path.
        walk_options.include_ignored = options.force;
        for entry in worktree.walk(ignore, &walk_options)? {
            if entry.kind == PathKind::Directory {
                continue;
            }
            files.insert(entry.path);
        }
    }

    // ── `-u`: the paths the index already knows about, in scope ─────────────────────────────
    if options.update {
        let tracked: Vec<BString> = index
            .entries()
            .iter()
            .filter(|e| {
                e.stage() == Stage::Normal && !e.skip_worktree() && in_scope(&e.path, &specs)
            })
            .map(|e| e.path.clone())
            .collect();
        files.extend(tracked);
    }

    // ── staging ─────────────────────────────────────────────────────────────────────────────
    let index_mtime = worktree.index_mtime();
    let mut staged = BTreeSet::new();
    let mut sorted_files: Vec<BString> = files.into_iter().collect();
    sorted_files.sort();

    for path in sorted_files {
        if options.update && index.entry(BStr::new(&path)).is_none() {
            continue;
        }
        if options.skip_paths.contains(&path) {
            continue;
        }
        if !options.force && ignore.is_ignored(BStr::new(&path), PathKind::File)? {
            if named.contains(&path) {
                // Git's asymmetry: a named ignored path is an error, one found by a walk is
                // skipped. The rule is carried out so `check-ignore -v` can name it.
                let rule = match ignore.check(BStr::new(&path), PathKind::File)? {
                    IgnoreDecision::Ignored(pattern) => pattern.origin(),
                    _ => BString::default(),
                };
                return Err(Error::PathIgnored { path, rule });
            }
            continue;
        }
        let Some(stat) = stat_cache.stat(worktree, BStr::new(&path))?.cloned() else {
            continue;
        };
        if stat.mode == EntryMode::Tree {
            continue;
        }
        let existing = index.entry(BStr::new(&path)).cloned();
        if let Some(entry) = &existing {
            if entry.is_gitlink() {
                continue;
            }
            // Already current: Git writes nothing, and rewriting the stat data alone would make
            // the index look different from Git's for an operation that changed nothing.
            if !options.intent_to_add && stat.is_unchanged(entry, index_mtime) {
                continue;
            }
        }

        let oid = if options.intent_to_add {
            empty_blob
        } else if options.dry_run {
            worktree.hash_file_with(BStr::new(&path), &mut || {
                index_blob_has_cr(index, odb, BStr::new(&path))
            })?
        } else {
            let mut data = worktree.read_file(BStr::new(&path))?;
            if stat.mode != EntryMode::Link {
                data = worktree.clean_content(
                    BStr::new(&path),
                    data,
                    &mut || index_blob_has_cr(index, odb, BStr::new(&path)),
                    true,
                )?;
            }
            odb.write_raw(ObjectKind::Blob, &data)?
        };

        if !options.dry_run {
            let entry = build_index_entry(
                worktree.format(),
                BStr::new(&path),
                oid,
                &stat,
                stat.mode,
                Stage::Normal,
            );
            if index.stages(BStr::new(&path)).len() > 1 {
                // `add` during a merge is the resolution operation: every stage is dropped and a
                // single stage-0 entry replaces them.
                index.resolve(BStr::new(&path), entry)?;
            } else {
                index.insert(entry);
            }
        }

        if existing.is_some() {
            outcome.updated += 1;
        } else {
            outcome.added += 1;
        }
        staged.insert(path);
    }

    // ── deletions ───────────────────────────────────────────────────────────────────────────
    // Deliberately after staging: a path that was just written is present on disk, so it can
    // never be mistaken for a deletion even when the stat cache was stale.
    // A sparse-checkout placeholder has no file by design; its absence is not a deletion.
    let mut tracked_paths: Vec<BString> = index
        .entries()
        .iter()
        .filter(|e| !e.skip_worktree())
        .map(|e| e.path.clone())
        .collect();
    tracked_paths.sort();
    tracked_paths.dedup();

    for path in tracked_paths {
        if staged.contains(&path) {
            continue;
        }
        if !delete_scope.iter().any(|scope| is_under(&path, scope)) {
            continue;
        }
        if stat_cache.stat(worktree, BStr::new(&path))?.is_some() {
            continue;
        }
        let removed = if options.dry_run {
            index.stages(BStr::new(&path)).len()
        } else {
            let removed = index.remove_path(BStr::new(&path)).len();
            stat_cache.remove(BStr::new(&path));
            removed
        };
        if removed > 0 {
            outcome.removed += removed;
            staged.insert(path);
        }
    }

    if !options.dry_run {
        index.sort();
    }
    outcome.paths = staged.into_iter().collect();
    Ok(outcome)
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
    stat_cache: &mut StatCache,
    paths: &[BString],
    options: &RmOptions,
) -> Result<AddOutcome> {
    let mut outcome = AddOutcome::default();
    if paths.is_empty() {
        return Ok(outcome);
    }
    if !options.dry_run && !options.cached {
        worktree.require_root("rm")?;
    }

    let mut specs: Vec<BString> = Vec::new();
    for p in paths {
        specs.push(normalize_spec(BStr::new(p))?);
    }

    let mut tracked: Vec<BString> = index.entries().iter().map(|e| e.path.clone()).collect();
    tracked.sort();
    tracked.dedup();

    let mut unlink: Vec<BString> = Vec::new();
    let mut removed_dirs: Vec<BString> = Vec::new();
    let mut touched: BTreeSet<BString> = BTreeSet::new();

    for spec in &specs {
        let mut targets: Vec<BString> = Vec::new();
        if !index.stages(BStr::new(spec)).is_empty() {
            targets.push(spec.clone());
        } else {
            // Not a tracked path itself: it may be a directory that contains tracked files, and
            // that is the only case in which `-r` is required.
            let children: Vec<BString> = tracked
                .iter()
                .filter(|path| is_under(path, spec) && *path != spec)
                .cloned()
                .collect();
            if children.is_empty() {
                return Err(Error::PathNotFound(spec.clone()));
            }
            if !options.recursive {
                return Err(Error::Message(format!(
                    "not removing '{}' recursively without -r",
                    spec
                )));
            }
            targets.extend(children);
            // The work tree root itself is never removed, only what is in it.
            if !spec.is_empty() {
                removed_dirs.push(spec.clone());
            }
        }

        for path in targets {
            let stages = index.stages(BStr::new(&path));
            if stages.is_empty() {
                continue;
            }
            let mode = stages[0].mode;

            if !options.force && !options.cached {
                if mode == EntryMode::Commit {
                    if is_repository_dir(worktree, BStr::new(&path)) {
                        return Err(Error::Message(format!(
                            "submodule '{}' has local modifications (use --force to remove it)",
                            path
                        )));
                    }
                } else if stat_cache.stat(worktree, BStr::new(&path))?.is_some() {
                    // Present on disk: only the index's content may be thrown away. Deleting a
                    // file whose work tree content differs from stage 0 would lose work that
                    // neither the index nor any commit holds.
                    let staged_oid = stages
                        .iter()
                        .find(|e| e.stage() == Stage::Normal)
                        .map(|e| e.oid);
                    let worktree_oid = worktree.hash_file(BStr::new(&path))?;
                    if Some(worktree_oid) != staged_oid {
                        return Err(Error::Message(format!(
                            "'{}' has local modifications (use --force to remove it)",
                            path
                        )));
                    }
                }
            }

            let count = if options.dry_run {
                stages.len()
            } else {
                let count = index.remove_path(BStr::new(&path)).len();
                stat_cache.remove(BStr::new(&path));
                count
            };
            outcome.removed += count;
            if !options.dry_run && !options.cached {
                unlink.push(path.clone());
            }
            touched.insert(path);
        }
    }

    // The index is authoritative and has already been updated; the work tree follows. A failure
    // here leaves tracked deletions staged with the files still present, which `status` reports
    // and a second `rm` finishes.
    if !options.dry_run {
        for path in &unlink {
            let full = worktree.join(BStr::new(path))?;
            match std::fs::remove_file(&full) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::IoAtPath {
                        path: path.clone(),
                        source,
                    });
                }
            }
            // A directory the removal empties goes too, up to (not including) the work tree root.
            let mut parent: &[u8] = path.as_ref();
            while let Some(slash) = parent.iter().rposition(|&b| b == b'/') {
                parent = &parent[..slash];
                let dir = worktree.join(BStr::new(parent))?;
                if std::fs::remove_dir(&dir).is_err() {
                    break;
                }
                stat_cache.remove(BStr::new(parent));
            }
        }
        for dir in &removed_dirs {
            let full = worktree.join(BStr::new(dir))?;
            // Only empty directories go: an untracked file left behind keeps the directory, and
            // a failure (ENOTEMPTY) is not an error from `rm`'s point of view.
            let _ = std::fs::remove_dir(&full);
            stat_cache.remove(BStr::new(dir));
        }
        index.sort();
    }

    outcome.paths = touched.into_iter().collect();
    Ok(outcome)
}

// ── path helpers ─────────────────────────────────────────────────────────────────────────────

/// Strip the forms Git accepts for "this path" down to a repository-relative path.
///
/// A leading `./` and a trailing `/` are spelling, not location: `add ./src/` and `add src` name
/// the same tree. `.` alone becomes the empty spec, which callers treat as the whole work tree.
fn normalize_spec(spec: &BStr) -> Result<BString> {
    let mut bytes: &[u8] = spec.as_ref();
    while let Some(rest) = bytes.strip_prefix(b"./") {
        bytes = rest;
    }
    while bytes.len() > 1 && bytes.last() == Some(&b'/') {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes == b"." {
        bytes = b"";
    }
    let normalized = BString::from(bytes);
    check_relative_path(BStr::new(&normalized), false)?;
    Ok(normalized)
}

/// Whether `path` is `prefix` or sits below it. An empty prefix is the whole work tree.
///
/// Generic over the argument types on purpose: callers hold `&BString` (paths from an index) or
/// `&BStr` (specs) interchangeably, and both coerce through `AsRef<[u8]>` without allocating.
fn is_under(path: impl AsRef<[u8]>, prefix: impl AsRef<[u8]>) -> bool {
    let path = path.as_ref();
    let prefix = prefix.as_ref();
    if prefix.is_empty() || path == prefix {
        return true;
    }
    path.len() > prefix.len() && path.starts_with(prefix) && path[prefix.len()] == b'/'
}

/// Whether an index path falls inside any of the operation's specs.
fn in_scope(path: impl AsRef<[u8]>, specs: &[BString]) -> bool {
    let path = path.as_ref();
    specs.iter().any(|spec| is_under(path, spec))
}

/// Whether a directory is the root of another repository (a submodule or an embedded clone).
fn is_repository_dir(worktree: &Worktree, path: &BStr) -> bool {
    worktree
        .join(path)
        .map(|full| full.join(".git").exists())
        .unwrap_or(false)
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
    _format: ObjectFormat,
    path: &BStr,
    oid: crate::hash::ObjectId,
    stat: &crate::worktree::StatData,
    mode: EntryMode,
    stage: Stage,
) -> IndexEntry {
    IndexEntry {
        ctime_seconds: stat.ctime_seconds,
        ctime_nanos: stat.ctime_nanos,
        mtime_seconds: stat.mtime_seconds,
        mtime_nanos: stat.mtime_nanos,
        dev: stat.dev,
        ino: stat.ino,
        mode,
        uid: stat.uid,
        gid: stat.gid,
        size: stat.size,
        oid,
        flags: IndexEntryFlags::with(stage, path.len()),
        extended_flags: 0,
        path: path.to_owned(),
    }
}
