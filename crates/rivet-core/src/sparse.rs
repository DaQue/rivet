//! Sparse checkout: which tracked paths are present in the work tree.
//!
//! A sparse checkout is described by `core.sparseCheckout` (on or off), `core.sparseCheckoutCone`
//! (how the patterns are read) and the pattern file `info/sparse-checkout` of the git directory.
//! Both settings live in `config.worktree` once `extensions.worktreeConfig` is on, which is where
//! Git puts them. Paths outside the definition carry the `skip-worktree` bit in the index and have
//! no file in the work tree.
//!
//! # Caveats
//!
//! * **Cone mode is not gitignore matching.** The pattern file of a cone is a fixed shape (`/*`,
//!   `!/*/`, then a `/dir/` and `!/dir/*/` pair for every parent and a bare `/dir/` for every
//!   directory included recursively) that is read back into two sets of directories. Treating it as
//!   free-form patterns gives the same answer for a file Git wrote and a different one for a file
//!   somebody edited, and Git itself falls back to the free-form reading with a warning.
//! * **Free-form (non-cone) patterns decide per directory, top-down.** A directory that a pattern
//!   decides on (include or exclude) passes that verdict to everything below it, and a deeper
//!   directory or the file itself may overturn it with a pattern of its own. Nothing matching at
//!   all means excluded.
//! * **Only the main layout is handled**: no sparse index, no `check-rules`, and patterns given
//!   relative to a subdirectory are not rebased onto the root.

use std::collections::BTreeSet;
use std::path::PathBuf;

use bstr::{BStr, BString, ByteSlice};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::ignore::{IgnoreSource, Pattern};
use crate::index::{IndexEntryExtendedFlags, IndexEntryFlags};
use crate::object::EntryMode;
use crate::ops::{make_index_entry, prune_empty_dirs};
use crate::repo::Repository;
use crate::worktree::checkout::{remove_path, write_blob};

/// A parsed sparse-checkout definition.
#[derive(Clone, Debug)]
pub struct SparseCheckout {
    cone: bool,
    lines: Vec<BString>,
    patterns: Vec<Pattern>,
    parents: BTreeSet<BString>,
    recursive: BTreeSet<BString>,
}

/// What [`Repository::apply_sparse`] left alone, for the caller to report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SparseApply {
    /// Paths that should have gone but differ from the index, so their files were kept.
    pub dirty: Vec<BString>,
    /// Paths with conflict stages, which are never touched.
    pub unmerged: Vec<BString>,
    /// Paths that should have come back but have a file in the way.
    pub blocked: Vec<BString>,
    /// Cone mode: directories outside the cone, all of whose tracked files are gone, that still
    /// exist because of files git does not track.
    pub untracked_dirs: Vec<BString>,
}

/// Remove the backslashes that protect glob characters in a cone pattern.
fn unescape(text: &[u8]) -> BString {
    let mut out = Vec::with_capacity(text.len());
    let mut escaped = false;
    for &b in text {
        if b == b'\\' && !escaped {
            escaped = true;
            continue;
        }
        escaped = false;
        out.push(b);
    }
    BString::from(out)
}

/// Protect the glob characters in a directory name for a cone pattern.
fn escape(text: &[u8]) -> BString {
    let mut out = Vec::with_capacity(text.len());
    for &b in text {
        if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    BString::from(out)
}

/// The pattern lines of a cone that includes `directories` recursively: the two root lines, a pair
/// for every parent of an included directory, then one line per included directory.
///
/// A directory below another included one adds nothing (`c` and `c/d` give just `/c/`), and a
/// parent that is itself included is not a parent.
pub fn cone_patterns(directories: &BTreeSet<BString>) -> Vec<BString> {
    let mut recursive: BTreeSet<BString> = BTreeSet::new();
    for dir in directories {
        let covered = recursive
            .iter()
            .any(|kept| dir.len() > kept.len() && dir.starts_with(kept) && dir[kept.len()] == b'/');
        if !covered {
            recursive.insert(dir.clone());
        }
    }
    let mut parents: BTreeSet<BString> = BTreeSet::new();
    for dir in &recursive {
        let mut end = dir.len();
        while let Some(slash) = dir[..end].rfind_byte(b'/') {
            end = slash;
            let parent = BString::from(&dir[..end]);
            if !recursive.contains(&parent) {
                parents.insert(parent);
            }
        }
    }
    let mut lines = vec![BString::from("/*"), BString::from("!/*/")];
    for parent in &parents {
        let escaped = escape(parent);
        lines.push(BString::from(format!("/{escaped}/")));
        lines.push(BString::from(format!("!/{escaped}/*/")));
    }
    for dir in &recursive {
        lines.push(BString::from(format!("/{}/", escape(dir))));
    }
    lines
}

impl SparseCheckout {
    /// Read a definition from the contents of the pattern file.
    ///
    /// With `cone` set the file is expected to have the shape [`cone_patterns`] writes. A line that
    /// does not fit it makes the definition free-form instead, and each such line is reported in
    /// the returned warnings, in Git's words.
    pub fn parse(cone: bool, data: &[u8]) -> Result<(Self, Vec<String>)> {
        let lines: Vec<BString> = data
            .lines()
            .map(|l| BString::from(l.strip_suffix(b"\r").unwrap_or(l)))
            .collect();
        let mut warnings = Vec::new();
        let mut parents = BTreeSet::new();
        let mut recursive = BTreeSet::new();
        let mut cone_ok = cone;
        if cone {
            for line in &lines {
                if line.as_slice() == b"/*" || line.as_slice() == b"!/*/" {
                    continue;
                }
                let plain = |text: &[u8]| -> bool {
                    text.starts_with(b"/")
                        && text.ends_with(b"/")
                        && text.len() > 2
                        && !has_unescaped_glob(&text[1..text.len() - 1])
                };
                if let Some(rest) = line.strip_prefix(b"!")
                    && rest.ends_with(b"/*/")
                    && plain(&rest[..rest.len() - 2])
                {
                    let dir = unescape(&rest[1..rest.len() - 3]);
                    recursive.remove(&dir);
                    parents.insert(dir);
                } else if plain(line) {
                    recursive.insert(unescape(&line[1..line.len() - 1]));
                } else {
                    warnings.push(format!("warning: unrecognized pattern: '{line}'"));
                    cone_ok = false;
                    break;
                }
            }
            if !cone_ok {
                warnings.push("warning: disabling cone pattern matching".to_string());
            }
        }
        let mut patterns = Vec::new();
        if !cone_ok {
            patterns = Pattern::parse_file(data, IgnoreSource::InfoExclude, BStr::new(b""))?;
            parents.clear();
            recursive.clear();
        }
        Ok((
            Self {
                cone: cone_ok,
                lines,
                patterns,
                parents,
                recursive,
            },
            warnings,
        ))
    }

    /// Whether the definition is read as a cone.
    pub fn is_cone(&self) -> bool {
        self.cone
    }

    /// The lines of the pattern file.
    pub fn lines(&self) -> &[BString] {
        &self.lines
    }

    /// The directories a cone includes recursively, sorted and unescaped (what `list` prints).
    pub fn directories(&self) -> Vec<BString> {
        self.recursive.iter().cloned().collect()
    }

    /// Cone mode: the topmost directory on the way to `path` that is neither included nor a
    /// parent of an included one, i.e. the root of the part of the tree that is out of the cone.
    fn cone_boundary(&self, path: &BStr) -> Option<BString> {
        let components: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
        let mut prefix: Vec<u8> = Vec::new();
        for (i, component) in components[..components.len() - 1].iter().enumerate() {
            if i > 0 {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(component);
            if !self.parents.contains(prefix.as_slice().as_bstr()) {
                return Some(BString::from(prefix));
            }
        }
        None
    }

    /// Whether the tracked file `path` is part of the checkout.
    pub fn includes(&self, path: &BStr) -> bool {
        let components: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
        if self.cone {
            let mut prefix: Vec<u8> = Vec::new();
            for (i, component) in components[..components.len() - 1].iter().enumerate() {
                if i > 0 {
                    prefix.push(b'/');
                }
                prefix.extend_from_slice(component);
                if self.recursive.contains(prefix.as_slice().as_bstr()) {
                    return true;
                }
            }
            // A file directly in the root or in a parent directory is part of the cone.
            return prefix.is_empty() || self.parents.contains(prefix.as_slice().as_bstr());
        }
        // Every directory on the way passes its verdict down to what it contains, unless something
        // below speaks for itself.
        let mut verdict = false;
        let mut prefix: Vec<u8> = Vec::new();
        for (i, component) in components[..components.len() - 1].iter().enumerate() {
            if i > 0 {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(component);
            if let Some(decision) = self.last_match(BStr::new(&prefix), true) {
                verdict = decision;
            }
        }
        self.last_match(path, false).unwrap_or(verdict)
    }

    /// The verdict of the last pattern that matches exactly `path`: `Some(true)` to include it,
    /// `Some(false)` to exclude it, `None` when no pattern speaks.
    fn last_match(&self, path: &BStr, is_dir: bool) -> Option<bool> {
        self.patterns
            .iter()
            .rev()
            .find(|p| p.exact_match(path, is_dir))
            .map(|p| !p.negated)
    }
}

fn has_unescaped_glob(text: &[u8]) -> bool {
    let mut escaped = false;
    for &b in text {
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if matches!(b, b'*' | b'?' | b'[' | b']') {
            return true;
        }
    }
    false
}

impl Repository {
    fn worktree_config_path(&self) -> PathBuf {
        self.git_dir().join("config.worktree")
    }

    fn sparse_pattern_path(&self) -> PathBuf {
        self.git_dir().join("info").join("sparse-checkout")
    }

    /// A boolean setting from `config.worktree`, else from the repository configuration.
    fn sparse_setting(&self, key: &str) -> Result<Option<bool>> {
        let path = self.worktree_config_path();
        if path.is_file()
            && let Some(value) = Config::load(&path)?.get_bool(BStr::new(key))?
        {
            return Ok(Some(value));
        }
        self.config().get_bool(BStr::new(key))
    }

    /// Whether `core.sparseCheckout` is on.
    pub fn sparse_enabled(&self) -> Result<bool> {
        Ok(self.sparse_setting("core.sparseCheckout")?.unwrap_or(false))
    }

    /// The value of `core.sparseCheckoutCone`, if it was ever set.
    pub fn sparse_cone_setting(&self) -> Result<Option<bool>> {
        self.sparse_setting("core.sparseCheckoutCone")
    }

    /// The definition in force, or `None` when sparse checkout is off. The warnings are those of
    /// [`SparseCheckout::parse`].
    pub fn sparse_checkout(&self) -> Result<Option<(SparseCheckout, Vec<String>)>> {
        if !self.sparse_enabled()? {
            return Ok(None);
        }
        let cone = self.sparse_cone_setting()?.unwrap_or(false);
        let data = std::fs::read(self.sparse_pattern_path()).unwrap_or_default();
        SparseCheckout::parse(cone, &data).map(Some)
    }

    /// Record `core.sparseCheckout` and `core.sparseCheckoutCone` in `config.worktree`, turning on
    /// `extensions.worktreeConfig` first; with `disable_sparse_index` also `index.sparse = false`,
    /// which is what `disable` writes.
    pub fn write_sparse_settings(
        &mut self,
        enabled: bool,
        cone: bool,
        disable_sparse_index: bool,
    ) -> Result<()> {
        if self
            .config()
            .get_bool(BStr::new("extensions.worktreeConfig"))?
            != Some(true)
        {
            self.config_mut()
                .set(BStr::new("extensions.worktreeConfig"), b"true")?;
            self.write_config()?;
        }
        let path = self.worktree_config_path();
        let mut config = if path.is_file() {
            Config::load(&path)?
        } else {
            Config::new()
        };
        let text = |on: bool| if on { &b"true"[..] } else { &b"false"[..] };
        config.set(BStr::new("core.sparseCheckout"), text(enabled))?;
        config.set(BStr::new("core.sparseCheckoutCone"), text(cone))?;
        if disable_sparse_index {
            config.set(BStr::new("index.sparse"), b"false")?;
        }
        config.write(&path)
    }

    /// Replace the pattern file with `lines`.
    pub fn write_sparse_patterns(&self, lines: &[BString]) -> Result<()> {
        let path = self.sparse_pattern_path();
        let io = |source| Error::IoAtPath {
            path: BString::from(path.to_string_lossy().into_owned()),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let mut data = Vec::new();
        for line in lines {
            data.extend_from_slice(line);
            data.push(b'\n');
        }
        std::fs::write(&path, data).map_err(io)
    }

    /// Whether the pattern file exists.
    pub fn sparse_patterns_exist(&self) -> bool {
        self.sparse_pattern_path().is_file()
    }

    /// After a command that wrote files without looking at the definition (a merge), take the ones
    /// outside it out again. Warnings about files that had to stay are not worth a message there.
    pub(crate) fn reapply_sparse_quietly(&mut self) -> Result<()> {
        if let Some((sparse, _)) = self.sparse_checkout()? {
            self.apply_sparse(Some(&sparse))?;
        }
        Ok(())
    }

    /// Bring the index and the work tree in line with `sparse` (`None`: everything is present).
    ///
    /// A tracked path outside the definition loses its file and gets the `skip-worktree` bit, but
    /// only when the file is what the index says; a modified file stays, and is reported in
    /// [`SparseApply::dirty`]. A path inside the definition that has the bit gets its file back.
    pub fn apply_sparse(&mut self, sparse: Option<&SparseCheckout>) -> Result<SparseApply> {
        let root = self
            .worktree()?
            .require_root("sparse-checkout")?
            .to_path_buf();
        let mut report = SparseApply::default();
        let mut index = std::mem::take(self.index_mut());
        let mut changed = false;
        let result = (|| -> Result<()> {
            for entry in index.entries_mut().iter_mut() {
                let path = entry.path.clone();
                let path = BStr::new(&path[..]);
                let skipped = IndexEntryExtendedFlags::from_bits(entry.extended_flags)
                    .skip_worktree()
                    && entry.flags.is_extended();
                let wanted_out = sparse.is_some_and(|s| !s.includes(path));
                if wanted_out == skipped {
                    continue;
                }
                if entry.is_unmerged() {
                    report.unmerged.push(entry.path.clone());
                    continue;
                }
                if wanted_out {
                    if entry.mode != EntryMode::Commit
                        && let Some(stat) = self.worktree()?.stat(path)?
                    {
                        let clean = stat.mode != EntryMode::Tree
                            && self.worktree()?.hash_file(path)? == entry.oid
                            && stat.mode == entry.mode;
                        if !clean {
                            report.dirty.push(entry.path.clone());
                            continue;
                        }
                        remove_path(self.worktree()?, path)?;
                        prune_empty_dirs(&root, path);
                    }
                    set_skip(entry, true);
                } else {
                    if entry.mode != EntryMode::Commit {
                        match self.worktree()?.stat(path)? {
                            // A directory is in the way.
                            Some(stat) if stat.mode == EntryMode::Tree => {
                                report.blocked.push(entry.path.clone());
                                continue;
                            }
                            // A file reappeared: git keeps it and only clears the bit, so that
                            // `status` compares it with the index like any tracked file.
                            Some(_) => {}
                            None => {
                                write_blob(
                                    self.worktree()?,
                                    path,
                                    entry.oid,
                                    entry.mode,
                                    self.odb(),
                                )?;
                                let stat = self.worktree()?.stat(path)?;
                                let fresh =
                                    make_index_entry(path, entry.mode, entry.oid, stat.as_ref());
                                *entry = fresh;
                            }
                        }
                    }
                    set_skip(entry, false);
                }
                changed = true;
            }
            Ok(())
        })();
        if let Some(sparse) = sparse.filter(|s| s.cone) {
            let mut boundaries: BTreeSet<BString> = BTreeSet::new();
            for entry in index.entries() {
                if entry.skip_worktree()
                    && let Some(dir) = sparse.cone_boundary(BStr::new(&entry.path[..]))
                {
                    boundaries.insert(dir);
                }
            }
            for dir in boundaries {
                let prefix = format!("{dir}/");
                let all_skipped = index
                    .entries()
                    .iter()
                    .filter(|e| e.path.starts_with(prefix.as_bytes()))
                    .all(|e| e.skip_worktree());
                let on_disk = self
                    .worktree()
                    .ok()
                    .and_then(|w| w.join(BStr::new(&dir[..])).ok())
                    .is_some_and(|p| p.is_dir());
                if all_skipped && on_disk {
                    report.untracked_dirs.push(dir);
                }
            }
        }
        *self.index_mut() = index;
        if changed {
            self.write_index()?;
        }
        result?;
        Ok(report)
    }
}

/// Set or clear the `skip-worktree` bit of an index entry, keeping the extended flag word and the
/// extended bit consistent (the bit is dropped again when nothing else needs the word).
pub(crate) fn set_skip(entry: &mut crate::index::IndexEntry, on: bool) {
    let flags = IndexEntryExtendedFlags::from_bits(entry.extended_flags).set_skip_worktree(on);
    entry.extended_flags = flags.bits();
    let extended = flags.bits() != 0;
    entry.flags = IndexEntryFlags::from_bits(entry.flags.bits()).set_extended(extended);
}
