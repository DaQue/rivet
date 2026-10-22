//! `clean`: delete untracked files from the work tree.
//!
//! The candidates come from the same status machinery `status` uses (untracked and ignored
//! paths, so `.gitignore` is honoured), and the walk over the work tree decides what is printed
//! and removed: files in tracked directories one by one, untracked directories (`-d`) whole when
//! everything inside them is a candidate, otherwise file by file. Interactive mode (`-i`) and
//! `-e` patterns are not implemented.

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::{Repository, UntrackedMode};

use crate::cli::GlobalArgs;

/// Arguments to `rivet clean`.
#[derive(Debug, clap::Args)]
pub struct CleanArgs {
    /// Limit cleaning to these paths (default: the current directory).
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<PathBuf>,

    /// Show what would be removed and remove nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Really remove files; given twice, also remove nested repositories.
    #[arg(short = 'f', long = "force", action = clap::ArgAction::Count)]
    pub force: u8,

    /// Remove untracked directories as well as files.
    #[arg(short = 'd')]
    pub directories: bool,

    /// Also remove files that are ignored.
    #[arg(short = 'x')]
    pub ignored_too: bool,

    /// Remove only files that are ignored.
    #[arg(short = 'X')]
    pub ignored_only: bool,

    /// Interactive mode. Not implemented.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,
}

/// What the walk may remove.
struct Plan<'a> {
    repository: &'a Repository,
    root: PathBuf,
    untracked: BTreeSet<BString>,
    ignored_files: BTreeSet<BString>,
    ignored_dirs: Vec<BString>,
    tracked_files: BTreeSet<BString>,
    tracked_dirs: BTreeSet<BString>,
    specs: Vec<BString>,
    args: &'a CleanArgs,
    found: Vec<(BString, bool)>,
}

impl Plan<'_> {
    fn is_ignored(&self, path: &BStr) -> bool {
        self.ignored_files.contains(path)
            || self.ignored_dirs.iter().any(|d| {
                let d: &[u8] = d;
                path.starts_with(d)
            })
    }

    /// Whether the user's rules make this file a deletion candidate.
    fn candidate(&self, path: &BStr) -> bool {
        let ignored = self.is_ignored(path);
        if self.args.ignored_only {
            ignored
        } else if self.args.ignored_too {
            ignored || self.untracked.contains(path)
        } else {
            !ignored && self.untracked.contains(path)
        }
    }

    fn in_spec(&self, path: &BStr) -> bool {
        self.specs
            .iter()
            .any(|spec| rivet_core::ops::pathspec_matches(path, BStr::new(&spec[..])))
    }

    /// Whether `dir` (no trailing slash) holds only candidates, recursively, and is wholly
    /// inside the requested paths.
    fn collapsible(&self, dir: &Path, rel: &BStr) -> Result<bool> {
        if !self.in_spec(rel) {
            return Ok(false);
        }
        // With -X only ignored directories are removed whole; others are emptied file by file.
        if self.args.ignored_only {
            let mut as_dir = rel.to_vec();
            as_dir.push(b'/');
            if !self.ignored_dirs.iter().any(|d| as_dir.starts_with(d)) {
                return Ok(false);
            }
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let child_rel = join(rel, name.as_bytes());
            let child_rel = BStr::new(&child_rel[..]);
            if entry.file_type()?.is_dir() {
                if name.as_bytes() == b".git" {
                    return Ok(false);
                }
                if self.tracked_dirs.contains(child_rel) {
                    return Ok(false);
                }
                if entry.path().join(".git").exists() {
                    return Ok(false);
                }
                if !self.collapsible(&entry.path(), child_rel)? {
                    return Ok(false);
                }
            } else if !self.candidate(child_rel) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn walk(&mut self, dir: &Path, rel: &BStr, in_untracked_dir: bool) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name().as_bytes().to_vec());
        for entry in entries {
            let name = entry.file_name();
            if name.as_bytes() == b".git" {
                continue;
            }
            let child_rel = join(rel, name.as_bytes());
            let child = BStr::new(&child_rel[..]);
            if entry.file_type()?.is_dir() {
                if self.tracked_dirs.contains(child) {
                    self.walk(&entry.path(), child, false)?;
                    continue;
                }
                // An untracked directory: whole with -d; with -X its ignored files are still found.
                if !self.args.directories {
                    let mut as_dir = child_rel.to_vec();
                    as_dir.push(b'/');
                    let dir_ignored = self.ignored_dirs.iter().any(|d| as_dir.starts_with(d));
                    if self.args.ignored_only && !dir_ignored && !entry.path().join(".git").exists()
                    {
                        self.walk(&entry.path(), child, true)?;
                    }
                    continue;
                }
                if entry.path().join(".git").exists() {
                    if self.args.force >= 2 && !self.args.ignored_only && self.in_spec(child) {
                        self.found.push((child_rel.clone(), true));
                    }
                    continue;
                }
                if self.collapsible(&entry.path(), child)? {
                    self.found.push((child_rel.clone(), true));
                } else {
                    self.walk(&entry.path(), child, true)?;
                }
            } else if (!in_untracked_dir || self.args.directories || self.args.ignored_only)
                && !self.tracked_files.contains(child)
                && self.candidate(child)
                && self.in_spec(child)
            {
                self.found.push((child_rel.clone(), false));
            }
        }
        Ok(())
    }
}

fn join(dir: &BStr, name: &[u8]) -> BString {
    let mut out = BString::from(dir.as_bytes());
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend_from_slice(name);
    out
}

/// `path` as seen from the directory `cwd` (both work-tree relative, `/`-separated).
fn relative_to(cwd: &[u8], path: &[u8]) -> Vec<u8> {
    let base: Vec<&[u8]> = cwd
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let target: Vec<&[u8]> = path
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let shared = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut out = Vec::new();
    for _ in shared..base.len() {
        out.extend_from_slice(b"../");
    }
    let rest = target[shared..].join(&b'/');
    out.extend_from_slice(&rest);
    if path.ends_with(b"/") && !out.ends_with(b"/") {
        out.push(b'/');
    }
    out
}

/// Handler for `rivet clean`.
///
/// # Exit codes
///
/// * `0` — done (or nothing to do).
/// * `1` — some path could not be removed.
/// * `128` — neither `-n` nor `-f` was given (and `clean.requireForce` is not false), or an
///   unsupported option was used.
pub fn run_clean(global: &GlobalArgs, args: &CleanArgs) -> Result<i32> {
    if args.interactive {
        anyhow::bail!("clean: -i is not implemented");
    }
    let mut repository = super::open(global)?;
    let require_force = repository
        .config_value(BStr::new("clean.requireForce"))?
        .map(|v| rivet_core::parse_bool(BStr::new(&v[..])).unwrap_or(true))
        .unwrap_or(true);
    if require_force && !args.dry_run && args.force == 0 {
        anyhow::bail!("clean.requireForce is true and -f not given: refusing to clean");
    }
    if args.ignored_too && args.ignored_only {
        anyhow::bail!("clean: -x and -X cannot be used together");
    }
    let root = repository
        .worktree()?
        .root()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("clean: this operation must be run in a work tree"))?;

    let cwd_prefix = {
        let here = std::env::current_dir()?;
        let relative = here.strip_prefix(&root).unwrap_or(Path::new(""));
        BString::from(super::path_arg(relative)?)
    };
    let mut specs = Vec::new();
    for path in &args.paths {
        specs.push(super::worktree_path(&repository, path, "clean")?);
    }
    if specs.is_empty() {
        specs.push(if cwd_prefix.is_empty() {
            BString::from(".")
        } else {
            cwd_prefix.clone()
        });
    }

    let mut options = rivet_core::StatusOptions::new();
    options.untracked_files = UntrackedMode::All;
    options.ignored = true;
    options.refresh = false;
    let status = repository.status(&options)?;
    let mut ignored_files = BTreeSet::new();
    let mut ignored_dirs = Vec::new();
    for path in &status.ignored {
        if path.ends_with(b"/") {
            ignored_dirs.push(path.clone());
        } else {
            ignored_files.insert(path.clone());
        }
    }
    let mut tracked_files = BTreeSet::new();
    let mut tracked_dirs = BTreeSet::new();
    for entry in repository.index().entries() {
        tracked_files.insert(entry.path.clone());
        let mut end = 0;
        while let Some(slash) = entry.path[end..].find_byte(b'/') {
            end += slash;
            tracked_dirs.insert(BString::from(&entry.path[..end]));
            end += 1;
        }
    }

    let mut plan = Plan {
        repository: &repository,
        root: root.clone(),
        untracked: status.untracked.iter().cloned().collect(),
        ignored_files,
        ignored_dirs,
        tracked_files,
        tracked_dirs,
        specs,
        args,
        found: Vec::new(),
    };
    let _ = (&plan.repository, &plan.root);
    plan.walk(&root.clone(), BStr::new(""), false)?;

    let mut found = std::mem::take(&mut plan.found);
    found.sort_by(|a, b| display_key(a).cmp(&display_key(b)));

    use std::io::Write as _;
    let mut out = Vec::new();
    let mut failed = false;
    let shown = |path: &BString, dir: bool| {
        let mut full = path.to_vec();
        if dir {
            full.push(b'/');
        }
        rivet_core::ops::quote_path(&relative_to(&cwd_prefix, &full), false)
    };
    for (path, dir) in &found {
        let label = shown(path, *dir);
        if args.dry_run {
            out.extend_from_slice(b"Would remove ");
            out.extend_from_slice(&label);
            out.push(b'\n');
            continue;
        }
        let target = root.join(std::ffi::OsStr::from_bytes(path));
        let result = if *dir {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
        match result {
            Ok(()) => {
                if !global.quiet {
                    out.extend_from_slice(b"Removing ");
                    out.extend_from_slice(&label);
                    out.push(b'\n');
                }
            }
            Err(error) => {
                eprintln!("warning: failed to remove {}: {error}", target.display());
                failed = true;
            }
        }
    }
    std::io::stdout().write_all(&out)?;
    Ok(if failed { 1 } else { 0 })
}

fn display_key(item: &(BString, bool)) -> Vec<u8> {
    let mut key = item.0.to_vec();
    if item.1 {
        key.push(b'/');
    }
    key
}
