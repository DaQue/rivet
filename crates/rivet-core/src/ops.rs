//! High-level repository operations shared by the porcelain commands.
//!
//! `checkout`, `switch`, `reset`, `merge`, `pull` and `clone` all need the same handful of
//! moves — "who am I", "move `HEAD` and log it the way Git does", "make the index and work tree
//! match this tree" — and each of them must behave identically in every command that uses it.
//! They live here, on [`Repository`], so the CLI never re-implements repository logic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bstr::{BStr, BString, ByteSlice};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::index::{Index, IndexEntry, IndexEntryExtendedFlags, IndexEntryFlags, IndexExt, Stage};
use crate::object::{AnyObject, EntryMode, ObjectKind, Signature};
use crate::refs::{HeadState, RefName, ReflogEntry, ReflogMessage};
use crate::repo::Repository;
use crate::worktree::checkout::{
    CheckoutAction, CheckoutOptions, CheckoutPlan, remove_path, write_blob,
};
use crate::worktree::{StatCache, StatData};

/// The paths of the user-level configuration files Git reads, lowest priority first.
///
/// `$GIT_CONFIG_GLOBAL` replaces both when it is set; otherwise the XDG file is read before
/// `~/.gitconfig`, so the latter wins, which is Git's order.
pub fn global_config_paths() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        return vec![PathBuf::from(path)];
    }
    let mut paths = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) if !xdg.is_empty() => paths.push(PathBuf::from(xdg).join("git/config")),
        _ => {
            if let Some(home) = &home {
                paths.push(home.join(".config/git/config"));
            }
        }
    }
    if let Some(home) = home {
        paths.push(home.join(".gitconfig"));
    }
    paths
}

/// The system configuration file, unless `GIT_CONFIG_NOSYSTEM` disables it.
pub fn system_config_path() -> Option<PathBuf> {
    if std::env::var_os("GIT_CONFIG_NOSYSTEM").is_some_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    Some(
        std::env::var_os("GIT_CONFIG_SYSTEM")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/gitconfig")),
    )
}

/// The user's global and system configuration, lowest priority first. Missing files are skipped.
pub fn load_outer_configs() -> Result<Vec<Config>> {
    let mut out = Vec::new();
    let mut paths: Vec<PathBuf> = system_config_path().into_iter().collect();
    paths.extend(global_config_paths());
    for path in paths {
        if path.is_file() {
            out.push(Config::load(&path)?);
        }
    }
    Ok(out)
}

/// A config value from the repository file, then `~/.gitconfig`, then the system file.
///
/// `local` may be `None` for commands that run outside a repository (`init`, `clone`).
pub fn lookup_config(local: Option<&Config>, key: &BStr) -> Result<Option<BString>> {
    if let Some(local) = local
        && let Some(value) = local.get(key)?
    {
        return Ok(Some(value.to_owned()));
    }
    for config in load_outer_configs()?.iter().rev() {
        if let Some(value) = config.get(key)? {
            return Ok(Some(value.to_owned()));
        }
    }
    Ok(None)
}

/// The branch `init` and `clone` create when nobody named one: `init.defaultBranch`, else
/// `master`, which is still Git's built-in default.
pub fn default_branch_name() -> Result<BString> {
    Ok(lookup_config(None, BStr::new("init.defaultBranch"))?
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| BString::from("master")))
}

fn env_bytes(name: &str) -> Option<BString> {
    use std::os::unix::ffi::OsStringExt;
    std::env::var_os(name).map(|v| BString::from(v.into_vec()))
}

/// Which identity to resolve; the two read different environment variables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityKind {
    /// `GIT_AUTHOR_*`.
    Author,
    /// `GIT_COMMITTER_*`.
    Committer,
}

/// Resolve an identity from the environment, then the configuration, stamped with
/// `GIT_*_DATE` if set and the current time otherwise.
pub fn resolve_identity(local: Option<&Config>, kind: IdentityKind) -> Result<Signature> {
    let (name_var, email_var, date_var, what) = match kind {
        IdentityKind::Author => (
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_AUTHOR_DATE",
            "author",
        ),
        IdentityKind::Committer => (
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_COMMITTER_DATE",
            "committer",
        ),
    };
    let name = match env_bytes(name_var) {
        Some(v) => Some(v),
        None => lookup_config(local, BStr::new("user.name"))?,
    };
    let email = match env_bytes(email_var) {
        Some(v) => Some(v),
        None => match lookup_config(local, BStr::new("user.email"))? {
            Some(v) => Some(v),
            None => env_bytes("EMAIL"),
        },
    };
    let (Some(name), Some(email)) = (name, email) else {
        return Err(Error::Message(format!(
            "cannot resolve {what} identity: set user.name and user.email"
        )));
    };
    let date =
        env_bytes(date_var).and_then(|v| crate::object::commit::parse_git_date(BStr::new(&v)));
    Ok(match date {
        Some((when, tz_offset_minutes)) => Signature {
            name,
            email,
            when,
            tz_offset_minutes,
        },
        None => Signature::from_zoned(BStr::new(&name), BStr::new(&email), jiff::Zoned::now()),
    })
}

/// Build a stage-0 index entry, filling the stat fields from `stat` when the caller has them.
pub fn make_index_entry(
    path: &BStr,
    mode: EntryMode,
    oid: ObjectId,
    stat: Option<&StatData>,
) -> IndexEntry {
    let zero = StatData {
        ctime_seconds: 0,
        ctime_nanos: 0,
        mtime_seconds: 0,
        mtime_nanos: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
    };
    let stat = stat.copied().unwrap_or(zero);
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
        flags: IndexEntryFlags::with(Stage::Normal, path.len()),
        extended_flags: IndexEntryExtendedFlags::empty().bits(),
        path: path.to_owned(),
    }
}

/// Every blob and gitlink in a tree, keyed by full path.
pub fn flatten_tree(
    odb: &crate::odb::Odb,
    tree: ObjectId,
) -> Result<BTreeMap<BString, (EntryMode, ObjectId)>> {
    crate::merge::tree::flatten_tree(odb, tree)
}

/// Remove now-empty directories from `path`'s parent up to (not including) `root`.
pub fn prune_empty_dirs(root: &Path, path: &BStr) {
    let mut current: &[u8] = path.as_bytes();
    while let Some(slash) = current.rfind_byte(b'/') {
        current = &current[..slash];
        let dir = root.join(current.to_path_lossy());
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
    }
}

impl Repository {
    /// A configuration value with Git's precedence: this repository, then global, then system.
    pub fn config_value(&self, key: &BStr) -> Result<Option<BString>> {
        lookup_config(Some(self.config()), key)
    }

    /// Write the in-memory repository configuration back to `.git/config`.
    pub fn write_config(&self) -> Result<()> {
        self.config().write(&self.git_dir().join("config"))
    }

    /// The identity recorded as committer (and in reflog lines).
    pub fn committer_signature(&self) -> Result<Signature> {
        resolve_identity(Some(self.config()), IdentityKind::Committer)
    }

    /// The identity recorded as author.
    pub fn author_signature(&self) -> Result<Signature> {
        resolve_identity(Some(self.config()), IdentityKind::Author)
    }

    /// Whether an update to `name` should be written to its reflog, per
    /// `core.logAllRefUpdates`: unset means "yes" in a non-bare repository and "no" in a bare
    /// one, and when it is off only refs that already have a log keep getting entries.
    pub fn should_log_ref(&self, name: &RefName) -> bool {
        let setting = self
            .config()
            .get(BStr::new("core.logAllRefUpdates"))
            .ok()
            .flatten()
            .map(|v| v.to_ascii_lowercase());
        let all = match setting.as_deref() {
            Some(b"always") => return true,
            Some(value) => crate::config::parse_bool(BStr::new(value)).unwrap_or(false),
            None => !self.is_bare(),
        };
        if all
            && (name.is_head()
                || name.as_bytes().starts_with(b"refs/heads/")
                || name.as_bytes().starts_with(b"refs/remotes/")
                || name.as_bytes().starts_with(b"refs/notes/"))
        {
            return true;
        }
        crate::refs::reflog::exists(self.git_dir(), name)
    }

    /// Point a ref at `new` unconditionally, logging per [`Repository::should_log_ref`].
    pub fn set_ref_logged(
        &self,
        name: &RefName,
        new: ObjectId,
        who: &Signature,
        reason: &BStr,
    ) -> Result<()> {
        let message = self
            .should_log_ref(name)
            .then(|| ReflogMessage::new(who, reason));
        self.refs().set_direct(name, new, message)
    }

    /// Move whatever `HEAD` names to `new`.
    ///
    /// When `HEAD` is attached (or unborn) the branch moves and both the branch's and `HEAD`'s
    /// reflogs get the entry, which is what Git writes for `commit`, `reset`, `merge` and `pull`.
    /// When `HEAD` is detached, `HEAD` itself moves.
    pub fn move_head(&self, new: ObjectId, reason: &BStr) -> Result<()> {
        let head = self.head()?;
        let who = self.committer_signature()?;
        // A move to where the branch already is leaves its own log alone, but `HEAD`'s log
        // still records that the command ran.
        if head.oid() == Some(new) {
            let head_name = RefName::head();
            if self.should_log_ref(&head_name) {
                let entry = ReflogEntry::new(new, new, who, BString::from(reason));
                self.refs().append_reflog(&head_name, &entry)?;
            }
            return Ok(());
        }
        match &head {
            HeadState::Attached { branch, .. } | HeadState::Unborn { branch } => {
                let old = head.oid().unwrap_or_else(|| ObjectId::zero(self.format()));
                self.set_ref_logged(branch, new, &who, reason)?;
                let head_name = RefName::head();
                if self.should_log_ref(&head_name) {
                    let entry = ReflogEntry::new(old, new, who.clone(), BString::from(reason));
                    self.refs().append_reflog(&head_name, &entry)?;
                }
                Ok(())
            }
            HeadState::Detached { .. } => {
                let head_name = RefName::head();
                let message = self
                    .should_log_ref(&head_name)
                    .then(|| ReflogMessage::new(&who, reason));
                self.refs().detach_head(new, message)
            }
        }
    }

    /// Point `HEAD` at a branch (symbolically) or at a commit (detached), writing the
    /// `checkout: moving from X to Y` line Git writes on every switch.
    pub fn point_head(&self, target: HeadTarget<'_>, reason: &BStr) -> Result<()> {
        let who = self.committer_signature()?;
        let head_name = RefName::head();
        let message = self
            .should_log_ref(&head_name)
            .then(|| ReflogMessage::new(&who, reason));
        match target {
            HeadTarget::Branch(branch) => self.refs().set_head(branch, message),
            HeadTarget::Detached(oid) => self.refs().detach_head(oid, message),
        }
    }

    /// The tree a commit (or a tag that peels to one) records.
    pub fn commit_tree(&self, commit: ObjectId) -> Result<ObjectId> {
        let mut current = commit;
        for _ in 0..64 {
            let object = self.odb().read(&current)?;
            match object.parse()? {
                AnyObject::Commit(c) => return Ok(c.tree),
                AnyObject::Tree(_) => return Ok(current),
                AnyObject::Tag(t) => current = t.object,
                AnyObject::Blob(_) => {
                    return Err(Error::WrongObjectKind {
                        oid: Some(current),
                        expected: ObjectKind::Commit,
                        found: ObjectKind::Blob,
                    });
                }
            }
        }
        Err(Error::TagCycle { oid: current })
    }

    /// The tree `HEAD` records, or `None` on an unborn branch.
    pub fn head_tree_oid(&self) -> Result<Option<ObjectId>> {
        match self.head()?.oid() {
            Some(oid) => Ok(Some(self.commit_tree(oid)?)),
            None => Ok(None),
        }
    }

    /// Move the index and the work tree from `HEAD`'s tree to `to_tree`, the way
    /// `git checkout <branch>` does: local modifications in paths the two trees agree on are
    /// carried over, and a modification in a path that would change is refused.
    ///
    /// With `force`, local changes everywhere are discarded, as by `reset --hard`.
    ///
    /// Does not move `HEAD`; the caller does that after this succeeds.
    pub fn checkout_tree(&mut self, to_tree: ObjectId, force: bool) -> Result<()> {
        if force {
            return self.reset_hard_to_tree(to_tree);
        }
        let root = self.worktree()?.require_root("checkout")?.to_path_buf();
        if self.index().has_conflicts() {
            let path = self
                .index()
                .entries()
                .iter()
                .find(|e| e.is_unmerged())
                .map(|e| e.path.clone())
                .unwrap_or_default();
            return Err(Error::UnmergedPath { path });
        }
        let from_tree = self.head_tree_oid()?;
        let options = CheckoutOptions::new();
        let plan = CheckoutPlan::plan(self.format(), from_tree, to_tree, self.odb(), &options)?;
        plan.validate(
            self.worktree()?,
            self.index(),
            self.odb(),
            &mut StatCache::new(),
            &options,
        )?;
        plan.apply(self.worktree()?, self.odb())?;

        let mut updates = Vec::new();
        for entry in plan.entries() {
            let path = BStr::new(&entry.path[..]);
            match entry.action {
                CheckoutAction::Keep => {}
                CheckoutAction::Remove => {
                    prune_empty_dirs(&root, path);
                    updates.push((entry.path.clone(), None));
                }
                CheckoutAction::Create | CheckoutAction::Update => {
                    let (Some(oid), Some(mode)) = (entry.oid, entry.mode) else {
                        continue;
                    };
                    let stat = self.worktree()?.stat(path)?;
                    updates.push((
                        entry.path.clone(),
                        Some(make_index_entry(path, mode, oid, stat.as_ref())),
                    ));
                }
            }
        }
        let index = self.index_mut();
        for (path, entry) in updates {
            index.remove_path(BStr::new(&path[..]));
            if let Some(entry) = entry {
                index.insert(entry);
            }
        }
        index.sort();
        self.write_index()
    }

    /// Make the index exactly `tree`, keeping the cached stat data of entries that did not
    /// change so the next `status` does not have to rehash them (`reset --mixed`).
    pub fn reset_index_to_tree(&mut self, tree: Option<ObjectId>) -> Result<()> {
        let target = match tree {
            Some(tree) => flatten_tree(self.odb(), tree)?,
            None => BTreeMap::new(),
        };
        let old = std::mem::take(self.index_mut());
        let mut fresh = Index::new();
        for (path, (mode, oid)) in &target {
            let previous = old
                .entry(BStr::new(&path[..]))
                .filter(|e| e.stage() == Stage::Normal && e.oid == *oid && e.mode == *mode);
            let entry = match previous {
                Some(previous) => previous.clone(),
                None => make_index_entry(BStr::new(&path[..]), *mode, *oid, None),
            };
            fresh.insert(entry);
        }
        fresh.sort();
        *self.index_mut() = fresh;
        self.write_index()
    }

    /// The tree of every tracked path as it is in the work tree right now: index entries with
    /// their content re-read from disk, paths deleted from disk omitted. Staged additions are
    /// included. This is the tree of a stash commit. Nothing but objects is written.
    ///
    /// # Errors
    ///
    /// [`Error::UnmergedPath`] while the index has conflict stages, and I/O or object errors.
    pub fn worktree_tree(&self) -> Result<ObjectId> {
        let worktree = self.worktree()?;
        let mut tree_index = Index::new();
        for entry in self.index().entries() {
            if entry.stage() != Stage::Normal {
                return Err(Error::UnmergedPath {
                    path: entry.path.clone(),
                });
            }
            let path = BStr::new(&entry.path[..]);
            if entry.mode == EntryMode::Commit {
                tree_index.insert(entry.clone());
                continue;
            }
            let Some(stat) = worktree.stat(path)? else {
                continue;
            };
            if stat.mode == EntryMode::Tree {
                continue;
            }
            let data = worktree.read_file(path)?;
            let oid = self.odb().write_raw(ObjectKind::Blob, &data)?;
            tree_index.insert(make_index_entry(path, stat.mode, oid, Some(&stat)));
        }
        tree_index.sort();
        crate::repo::write_tree_of(self.odb(), &tree_index)
    }

    /// Make the index and the work tree exactly `tree`, discarding every local change to a
    /// tracked path (`reset --hard`, `checkout -f`). Untracked files are left alone unless the
    /// tree has a file at their path, which Git overwrites too.
    pub fn reset_hard_to_tree(&mut self, tree: ObjectId) -> Result<()> {
        let root = self.worktree()?.require_root("reset --hard")?.to_path_buf();
        let target = flatten_tree(self.odb(), tree)?;
        let old = std::mem::take(self.index_mut());

        // Removals first, so a file replaced by a directory of the same name has room.
        let mut removed: Vec<BString> = Vec::new();
        for entry in old.entries() {
            if target.contains_key(&entry.path) || removed.last() == Some(&entry.path) {
                continue;
            }
            let path = BStr::new(&entry.path[..]);
            if self.worktree()?.stat(path)?.is_some() {
                remove_path(self.worktree()?, path)?;
                prune_empty_dirs(&root, path);
            }
            removed.push(entry.path.clone());
        }

        let mut fresh = Index::new();
        for (path, (mode, oid)) in &target {
            let path_ref = BStr::new(&path[..]);
            if *mode == EntryMode::Commit {
                fresh.insert(make_index_entry(path_ref, *mode, *oid, None));
                continue;
            }
            let on_disk = self.worktree()?.stat(path_ref)?;
            let unchanged = match (&on_disk, old.entry(path_ref)) {
                (Some(stat), Some(previous)) => {
                    previous.stage() == Stage::Normal
                        && previous.oid == *oid
                        && previous.mode == *mode
                        && stat.matches_entry(previous)
                }
                _ => false,
            };
            let stat = if unchanged {
                on_disk
            } else {
                // A directory in the way of a file is removed, as Git does when the tree says
                // the path is a file.
                let full = self.worktree()?.join(path_ref)?;
                if full.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                    std::fs::remove_dir_all(&full).map_err(|e| Error::IoAtPath {
                        path: path.clone(),
                        source: e,
                    })?;
                }
                write_blob(self.worktree()?, path_ref, *oid, *mode, self.odb())?;
                self.worktree()?.stat(path_ref)?
            };
            fresh.insert(make_index_entry(path_ref, *mode, *oid, stat.as_ref()));
        }
        fresh.sort();
        *self.index_mut() = fresh;
        self.write_index()
    }
}

/// Whether `path` is selected by a command-line pathspec: the path itself, anything under it
/// when it names a directory, or everything for `.`.
pub fn pathspec_matches(path: &BStr, spec: &BStr) -> bool {
    if spec.is_empty() || spec == "." {
        return true;
    }
    let spec = spec.strip_suffix(b"/").unwrap_or(spec);
    path == spec || (path.starts_with(spec) && path.get(spec.len()) == Some(&b'/'))
}

impl Repository {
    /// The parents a commit records, in order.
    pub fn commit_parents(&self, commit: ObjectId) -> Result<Vec<ObjectId>> {
        let object = self.odb().read_typed(&commit, ObjectKind::Commit)?;
        match object.parse()? {
            AnyObject::Commit(c) => Ok(c.parents),
            _ => Ok(Vec::new()),
        }
    }

    /// Write a commit object for `tree` with the given parents and message, using the resolved
    /// author and committer identities. No ref moves; the caller decides how to log that.
    pub fn write_commit_object(
        &self,
        tree: ObjectId,
        parents: &[ObjectId],
        message: &BStr,
    ) -> Result<ObjectId> {
        let mut builder = crate::object::CommitBuilder::new();
        builder.tree(tree);
        for parent in parents {
            builder.parent(*parent);
        }
        builder.author(self.author_signature()?);
        builder.committer(self.committer_signature()?);
        builder.message(message);
        let body = builder.build(self.format())?;
        self.odb().write_raw(ObjectKind::Commit, &body)
    }

    /// Whether `ancestor` is reachable from `descendant` (a commit is its own ancestor), as
    /// `git merge-base --is-ancestor` answers it.
    pub fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::from([descendant]);
        while let Some(oid) = queue.pop_front() {
            if oid == ancestor {
                return Ok(true);
            }
            if !seen.insert(oid) {
                continue;
            }
            for parent in self.commit_parents(oid)? {
                queue.push_back(parent);
            }
        }
        Ok(false)
    }

    /// Copy the paths `pathspecs` select from `source` (a tree, or the index when `None`) into
    /// the index (`staged`) and/or the work tree (`worktree`).
    ///
    /// Without `overlay`, a selected path that is absent from the source is deleted from each
    /// destination, which is `git restore`'s behaviour; with it, such paths are left alone, which
    /// is `git checkout <tree-ish> -- <paths>`. Every pathspec must select something, or nothing
    /// is written and the first unmatched one is [`Error::PathNotFound`]. Returns the number of
    /// paths written to the work tree, or to the index when only the index is the destination.
    pub fn restore_paths(
        &mut self,
        source: Option<ObjectId>,
        pathspecs: &[BString],
        staged: bool,
        worktree: bool,
        overlay: bool,
    ) -> Result<usize> {
        let root = self.worktree()?.require_root("restore")?.to_path_buf();
        let source_map: BTreeMap<BString, (EntryMode, ObjectId)> = match source {
            Some(tree) => flatten_tree(self.odb(), tree)?,
            None => self
                .index()
                .entries()
                .iter()
                .filter(|e| e.stage() == Stage::Normal)
                .map(|e| (e.path.clone(), (e.mode, e.oid)))
                .collect(),
        };
        let mut candidates: std::collections::BTreeSet<BString> =
            source_map.keys().cloned().collect();
        if !overlay {
            for entry in self.index().entries() {
                candidates.insert(entry.path.clone());
            }
        }
        let selected: Vec<BString> = candidates
            .into_iter()
            .filter(|path| {
                pathspecs
                    .iter()
                    .any(|spec| pathspec_matches(BStr::new(&path[..]), BStr::new(&spec[..])))
            })
            .collect();
        for spec in pathspecs {
            if !selected
                .iter()
                .any(|path| pathspec_matches(BStr::new(&path[..]), BStr::new(&spec[..])))
            {
                return Err(Error::PathNotFound(spec.clone()));
            }
        }

        let mut written = 0usize;
        for path in &selected {
            let path_ref = BStr::new(&path[..]);
            let src = source_map.get(path).copied();
            if worktree {
                match src {
                    Some((EntryMode::Commit, _)) => {}
                    Some((mode, oid)) => {
                        write_blob(self.worktree()?, path_ref, oid, mode, self.odb())?;
                        written += 1;
                    }
                    None => {
                        if self.worktree()?.stat(path_ref)?.is_some() {
                            remove_path(self.worktree()?, path_ref)?;
                            prune_empty_dirs(&root, path_ref);
                        }
                    }
                }
            }
            if staged {
                let stat = if worktree {
                    self.worktree()?.stat(path_ref)?
                } else {
                    None
                };
                let previous = self
                    .index()
                    .entry(path_ref)
                    .filter(|e| e.stage() == Stage::Normal)
                    .cloned();
                let index = self.index_mut();
                index.remove_path(path_ref);
                if let Some((mode, oid)) = src {
                    let entry = match (&stat, previous) {
                        (Some(stat), _) => make_index_entry(path_ref, mode, oid, Some(stat)),
                        (None, Some(previous)) if previous.oid == oid && previous.mode == mode => {
                            previous
                        }
                        _ => make_index_entry(path_ref, mode, oid, None),
                    };
                    index.insert(entry);
                }
                if !worktree {
                    written += 1;
                }
            } else if worktree {
                // The index is not a destination, but a file just rewritten from the index has
                // new stat data; refreshing it saves the next `status` a rehash.
                if let Some((mode, oid)) = src {
                    let stat = self.worktree()?.stat(path_ref)?;
                    let refresh = self.index().entry(path_ref).is_some_and(|e| {
                        e.stage() == Stage::Normal && e.oid == oid && e.mode == mode
                    });
                    if refresh {
                        let index = self.index_mut();
                        index.remove_path(path_ref);
                        index.insert(make_index_entry(path_ref, mode, oid, stat.as_ref()));
                    }
                }
            }
        }
        self.index_mut().sort();
        self.write_index()?;
        Ok(written)
    }

    /// Three-way merge `theirs` into the index and work tree on top of `HEAD`.
    ///
    /// `base` is the merge base (or `None` for unrelated histories). Refuses with
    /// [`Error::DirtyWorktree`] when a path the merge changes has staged or unstaged local
    /// modifications, and with [`Error::UntrackedOverwrite`] when it would write over an
    /// untracked file, before touching anything. On return the index holds the clean results and
    /// the conflict stages, and the work tree holds the merged files with conflict markers.
    ///
    /// The returned merge's `paths` are only those whose content changed relative to `HEAD`.
    pub fn merge_into_worktree(
        &mut self,
        base: Option<ObjectId>,
        theirs: ObjectId,
        options: &crate::merge::MergeOptions,
    ) -> Result<crate::merge::TreeMerge> {
        let root = self.worktree()?.require_root("merge")?.to_path_buf();
        let ours_commit = self
            .head()?
            .oid()
            .ok_or_else(|| Error::Message("cannot merge into an unborn branch".to_string()))?;
        let ours_tree = self.commit_tree(ours_commit)?;
        let theirs_tree = self.commit_tree(theirs)?;
        let base_tree = match base {
            Some(base) => Some(self.commit_tree(base)?),
            None => None,
        };
        let mut result =
            crate::merge::merge_trees(self.odb(), base_tree, ours_tree, theirs_tree, options)?;
        let ours_map = flatten_tree(self.odb(), ours_tree)?;
        result.paths.retain(|p| {
            let ours = ours_map.get(&p.path).map(|(_, oid)| *oid);
            p.kind.is_some() || ours != p.result
        });

        // Safety first: nothing the merge writes may hold work the user has not committed.
        let mut dirty = Vec::new();
        let mut obstructed = Vec::new();
        let touched: Vec<BString> = result
            .paths
            .iter()
            .map(|p| p.path.clone())
            .chain(result.conflicts.iter().map(|c| c.path.clone()))
            .collect();
        for path in &touched {
            let path_ref = BStr::new(&path[..]);
            let head_entry = ours_map.get(path);
            let index_entry = self.index().entry(path_ref).cloned();
            // Local work that already equals what the merge would write is not at risk.
            if !result.conflicts.iter().any(|c| c.path == *path)
                && let Some(merged) = result.paths.iter().find(|p| p.path == *path)
            {
                let on_disk = self.worktree()?.stat(path_ref)?.is_some();
                let same = match (&index_entry, merged.result) {
                    (Some(entry), Some(oid)) => {
                        on_disk
                            && entry.stage() == Stage::Normal
                            && entry.oid == oid
                            && self.worktree()?.hash_file(path_ref)? == oid
                    }
                    (None, None) => !on_disk,
                    _ => false,
                };
                if same {
                    continue;
                }
            }
            match (&index_entry, head_entry) {
                (None, None) => {
                    if self.worktree()?.stat(path_ref)?.is_some() {
                        obstructed.push(path.clone());
                    }
                    continue;
                }
                (Some(entry), Some((mode, oid))) if entry.oid == *oid && entry.mode == *mode => {}
                _ => {
                    dirty.push(path.clone());
                    continue;
                }
            }
            if let Some(entry) = index_entry
                && self.worktree()?.stat(path_ref)?.is_some()
                && self.worktree()?.hash_file(path_ref)? != entry.oid
            {
                dirty.push(path.clone());
            }
        }
        dirty.sort();
        dirty.dedup();
        obstructed.sort();
        obstructed.dedup();
        match (dirty.is_empty(), obstructed.is_empty()) {
            (true, true) => {}
            (false, true) => return Err(Error::DirtyWorktree { paths: dirty }),
            (true, false) if obstructed.len() == 1 => {
                return Err(Error::UntrackedOverwrite(obstructed.remove(0)));
            }
            (_, _) => {
                return Err(Error::MergeBlocked {
                    dirty,
                    untracked: obstructed,
                });
            }
        }

        let mut index = std::mem::take(self.index_mut());
        crate::merge::tree::update_index_after_merge(&mut index, self.odb(), &result)?;
        crate::merge::write_conflict_stages(&mut index, &result.conflicts)?;
        index.sort();
        *self.index_mut() = index;
        crate::merge::write_merged_worktree(
            self.worktree()?,
            self.index(),
            self.odb(),
            &result,
            options,
        )?;

        // Refresh stat data for the clean paths just written, and tidy deleted ones.
        let conflicted: std::collections::BTreeSet<BString> =
            result.conflicts.iter().map(|c| c.path.clone()).collect();
        for path_merge in &result.paths {
            if conflicted.contains(&path_merge.path) {
                continue;
            }
            let path_ref = BStr::new(&path_merge.path[..]);
            match path_merge.result {
                None => prune_empty_dirs(&root, path_ref),
                Some(oid) => {
                    let stat = self.worktree()?.stat(path_ref)?;
                    let mode = self.index().entry(path_ref).map(|e| e.mode);
                    if let Some(mode) = mode {
                        let index = self.index_mut();
                        index.remove_path(path_ref);
                        index.insert(make_index_entry(path_ref, mode, oid, stat.as_ref()));
                    }
                }
            }
        }
        self.index_mut().sort();
        self.write_index()?;
        Ok(result)
    }
}

/// What [`Repository::point_head`] makes `HEAD` refer to.
#[derive(Clone, Copy, Debug)]
pub enum HeadTarget<'a> {
    /// Attach `HEAD` to a branch (which may be unborn).
    Branch(&'a RefName),
    /// Detach `HEAD` at a commit.
    Detached(ObjectId),
}

/// Format a signature's time the way `git log` prints it by default:
/// `Tue Nov 14 23:13:20 2023 +0100`, in the signature's own zone.
pub fn format_git_date(when: i64, tz_offset_minutes: i32) -> String {
    let offset =
        jiff::tz::Offset::from_seconds(tz_offset_minutes * 60).unwrap_or(jiff::tz::Offset::UTC);
    let stamp = jiff::Timestamp::from_second(when).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    let zoned = stamp.to_zoned(jiff::tz::TimeZone::fixed(offset));
    let abs = tz_offset_minutes.abs();
    format!(
        "{} {}{:02}{:02}",
        zoned.strftime("%a %b %-d %H:%M:%S %Y"),
        if tz_offset_minutes < 0 { '-' } else { '+' },
        abs / 60,
        abs % 60
    )
}

/// Whether `style` names a date format [`format_date_style`] understands.
pub fn is_date_style(style: &str) -> bool {
    matches!(
        style,
        "default"
            | "iso"
            | "iso8601"
            | "iso-strict"
            | "iso8601-strict"
            | "relative"
            | "short"
            | "raw"
            | "unix"
            | "rfc"
            | "rfc2822"
    )
}

/// Format a signature time in one of `--date=`'s styles: `default`, `iso`, `iso-strict`,
/// `relative`, `short`, `raw`, `unix`, `rfc2822`. `now` is the reference for `relative`.
/// Returns `None` for an unknown style.
pub fn format_date_style(
    style: &str,
    when: i64,
    tz_offset_minutes: i32,
    now: i64,
) -> Option<String> {
    let offset =
        jiff::tz::Offset::from_seconds(tz_offset_minutes * 60).unwrap_or(jiff::tz::Offset::UTC);
    let stamp = jiff::Timestamp::from_second(when).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    let zoned = stamp.to_zoned(jiff::tz::TimeZone::fixed(offset));
    let abs = tz_offset_minutes.abs();
    let sign = if tz_offset_minutes < 0 { '-' } else { '+' };
    let zone = format!("{sign}{:02}{:02}", abs / 60, abs % 60);
    Some(match style {
        "default" => format_git_date(when, tz_offset_minutes),
        "iso" | "iso8601" => format!("{} {zone}", zoned.strftime("%Y-%m-%d %H:%M:%S")),
        "iso-strict" | "iso8601-strict" => {
            let zone = if tz_offset_minutes == 0 {
                "Z".to_string()
            } else {
                format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
            };
            format!("{}{zone}", zoned.strftime("%Y-%m-%dT%H:%M:%S"))
        }
        "short" => zoned.strftime("%Y-%m-%d").to_string(),
        "raw" => format!("{when} {zone}"),
        "unix" => when.to_string(),
        "rfc" | "rfc2822" => format!("{} {zone}", zoned.strftime("%a, %-d %b %Y %H:%M:%S")),
        "relative" => relative_date(when, now),
        _ => return None,
    })
}

/// Git's `show_date_relative`.
fn relative_date(when: i64, now: i64) -> String {
    fn unit(n: i64, one: &str, many: &str) -> String {
        format!("{n} {} ago", if n == 1 { one } else { many })
    }
    if now < when {
        return "in the future".to_string();
    }
    let mut diff = now - when;
    if diff < 90 {
        return unit(diff, "second", "seconds");
    }
    diff = (diff + 30) / 60;
    if diff < 90 {
        return unit(diff, "minute", "minutes");
    }
    diff = (diff + 30) / 60;
    if diff < 36 {
        return unit(diff, "hour", "hours");
    }
    diff = (diff + 12) / 24;
    if diff < 14 {
        return unit(diff, "day", "days");
    }
    if diff < 70 {
        return unit((diff + 3) / 7, "week", "weeks");
    }
    if diff < 365 {
        return unit((diff + 15) / 30, "month", "months");
    }
    if diff < 1825 {
        let months_total = (diff * 12 * 2 + 365) / (365 * 2);
        let (years, months) = (months_total / 12, months_total % 12);
        if months > 0 {
            return format!(
                "{} {}, {}",
                years,
                if years == 1 { "year" } else { "years" },
                unit(months, "month", "months")
            );
        }
        return unit(years, "year", "years");
    }
    unit((diff + 183) / 365, "year", "years")
}

/// Parse a `--since`/`--until` style date (`@epoch`, `epoch`, ISO date or datetime) to seconds.
pub fn parse_approxidate(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('@') {
        return rest.split_whitespace().next()?.parse().ok();
    }
    if let Ok(n) = text.parse::<i64>()
        && text.len() >= 9
    {
        return Some(n);
    }
    crate::object::commit::parse_git_date(BStr::new(text.as_bytes())).map(|(when, _)| when)
}

/// Git's C-style path quoting (`core.quotePath` on): a path with a control character, `"`, `\`
/// or a byte outside ASCII is wrapped in double quotes with C escapes and octal bytes; any other
/// path is returned as is. With `quote_space` a path containing a space is quoted too, which is
/// how `status --short` shows it.
pub fn quote_path(path: &[u8], quote_space: bool) -> BString {
    let needs = path
        .iter()
        .any(|b| *b < 0x20 || *b == b'"' || *b == b'\\' || *b >= 0x7f)
        || (quote_space && path.contains(&b' '));
    if !needs {
        return BString::from(path);
    }
    let mut out = BString::from("\"");
    for &b in path {
        match b {
            7 => out.extend_from_slice(b"\\a"),
            8 => out.extend_from_slice(b"\\b"),
            9 => out.extend_from_slice(b"\\t"),
            10 => out.extend_from_slice(b"\\n"),
            11 => out.extend_from_slice(b"\\v"),
            12 => out.extend_from_slice(b"\\f"),
            13 => out.extend_from_slice(b"\\r"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b if !(0x20..0x7f).contains(&b) => {
                out.extend_from_slice(format!("\\{b:03o}").as_bytes())
            }
            b => out.push(b),
        }
    }
    out.push(b'"');
    out
}
