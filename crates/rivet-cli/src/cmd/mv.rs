//! `mv`: rename files or directories in the work tree and in the index.

use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use anyhow::Result;
use bstr::{BStr, BString};
use rivet_core::{IndexExt as _, Repository};

use crate::cli::GlobalArgs;

/// Arguments to `rivet mv`.
#[derive(Debug, clap::Args)]
pub struct MvArgs {
    /// The sources, then the destination (a directory when there are several sources).
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Move even when the destination exists.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Show what would be renamed and change nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Skip moves that would fail instead of stopping.
    #[arg(short = 'k')]
    pub skip_errors: bool,
}

/// One rename: the repository-relative paths and whether it is a work tree move or just the
/// index entry of a file inside a directory that is moved as a whole.
struct Move {
    src: BString,
    dst: BString,
    index_only: bool,
}

/// Handler for `rivet mv`.
///
/// # Exit codes
///
/// * `0` — everything moved (or was skipped under `-k`).
/// * `128` — a source is missing, untracked or conflicted, a destination exists (without `-f`),
///   or a rename failed. Nothing is moved when the check phase fails.
pub fn run_mv(global: &GlobalArgs, args: &MvArgs) -> Result<i32> {
    if args.paths.len() < 2 {
        eprintln!("usage: git mv [-v] [-f] [-n] [-k] <source> <destination>");
        eprintln!("   or: git mv [-v] [-f] [-n] [-k] <source>... <destination-directory>");
        return Ok(129);
    }
    let mut repository = super::open(global)?;
    let root = repository
        .worktree()?
        .root()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("mv: this operation must be run in a work tree"))?;

    let (sources, destination) = args.paths.split_at(args.paths.len() - 1);
    let destination = &destination[0];
    let dest_repo = super::worktree_path(&repository, destination, "mv")?;
    let dest_is_dir = root
        .join(std::ffi::OsStr::from_bytes(&dest_repo))
        .symlink_metadata()
        .is_ok_and(|m| m.is_dir());
    if sources.len() > 1 && !dest_is_dir {
        anyhow::bail!("destination '{}' is not a directory", destination.display());
    }

    let mut moves: Vec<Move> = Vec::new();
    let mut shown: Vec<(String, String)> = Vec::new();
    for source in sources {
        let src = super::worktree_path(&repository, source, "mv")?;
        let into_dir = dest_is_dir;
        let base = source
            .file_name()
            .map(|n| n.as_bytes().to_vec())
            .unwrap_or_default();
        let dst = if into_dir {
            let mut joined = if dest_repo == "." {
                Vec::new()
            } else {
                let mut d = dest_repo.to_vec();
                d.push(b'/');
                d
            };
            joined.extend_from_slice(&base);
            BString::from(joined)
        } else {
            dest_repo.clone()
        };
        let dst_shown = dst.to_string();
        let src_shown = src.to_string();

        let fail = |why: &str| -> Result<bool> {
            if args.skip_errors {
                return Ok(true);
            }
            anyhow::bail!("{why}, source={src_shown}, destination={dst_shown}")
        };
        let src_path = root.join(std::ffi::OsStr::from_bytes(&src));
        let dst_path = root.join(std::ffi::OsStr::from_bytes(&dst));
        let Ok(meta) = src_path.symlink_metadata() else {
            fail("bad source")?;
            continue;
        };
        let dst_parent_ok = dst_path.parent().is_none_or(std::path::Path::is_dir);
        if !dst_parent_ok {
            fail("destination directory does not exist")?;
            continue;
        }
        let into_self = dst.starts_with(&src[..]) && dst.get(src.len()).is_none_or(|c| *c == b'/');
        if into_self {
            fail("can not move directory into itself")?;
            continue;
        }
        let dst_meta = dst_path.symlink_metadata().ok();
        let mut file_moves = Vec::new();
        if meta.is_dir() {
            if dst_meta.as_ref().is_some_and(|m| !m.is_dir()) {
                fail("cannot move directory over file")?;
                continue;
            }
            let prefix = [&src[..], b"/"].concat();
            for entry in repository.index().entries() {
                if entry.path.starts_with(&prefix) {
                    let mut new = dst.to_vec();
                    new.push(b'/');
                    new.extend_from_slice(&entry.path[prefix.len()..]);
                    file_moves.push(Move {
                        src: entry.path.clone(),
                        dst: BString::from(new),
                        index_only: true,
                    });
                }
            }
            if file_moves.is_empty() {
                fail("source directory is empty")?;
                continue;
            }
        } else {
            let entries = repository.index().stages(BStr::new(&src[..]));
            if entries.is_empty() {
                fail("not under version control")?;
                continue;
            }
            if entries.iter().any(|e| e.is_unmerged()) {
                fail("conflicted")?;
                continue;
            }
            if dst_meta.is_some() {
                if !args.force {
                    fail("destination exists")?;
                    continue;
                }
                if dst_meta.as_ref().is_some_and(|m| m.is_dir()) {
                    fail("destination exists")?;
                    continue;
                }
                eprintln!("warning: overwriting '{dst_shown}'");
            }
        }
        shown.push((src_shown.clone(), dst_shown.clone()));
        moves.push(Move {
            src: src.clone(),
            dst: dst.clone(),
            index_only: false,
        });
        for file_move in file_moves {
            shown.push((
                String::from_utf8_lossy(&file_move.src).into_owned(),
                String::from_utf8_lossy(&file_move.dst).into_owned(),
            ));
            moves.push(file_move);
        }
    }

    if args.dry_run {
        for (src, dst) in &shown {
            println!("Checking rename of '{src}' to '{dst}'");
        }
    }
    for (src, dst) in &shown {
        if args.dry_run || global.verbose {
            println!("Renaming {src} to {dst}");
        }
    }
    if args.dry_run {
        return Ok(0);
    }

    for mv in &moves {
        if mv.index_only {
            continue;
        }
        let from = root.join(std::ffi::OsStr::from_bytes(&mv.src));
        let to = root.join(std::ffi::OsStr::from_bytes(&mv.dst));
        if let Err(error) = std::fs::rename(&from, &to) {
            anyhow::bail!("renaming '{}' failed: {error}", from.display());
        }
    }
    update_index(&mut repository, &moves)?;
    Ok(0)
}

fn update_index(repository: &mut Repository, moves: &[Move]) -> Result<()> {
    let mut moved = Vec::new();
    for mv in moves {
        let path = BStr::new(&mv.src[..]);
        // A directory entry has no index entry of its own.
        let Some(entry) = repository.index().entry(path).cloned() else {
            continue;
        };
        repository.index_mut().remove_path(path);
        moved.push((mv.dst.clone(), entry));
    }
    for (dst, entry) in moved {
        let path = BStr::new(&dst[..]);
        repository.index_mut().remove_path(path);
        let stat = repository.worktree()?.stat(path)?;
        repository
            .index_mut()
            .insert(rivet_core::ops::make_index_entry(
                path,
                entry.mode,
                entry.oid,
                stat.as_ref(),
            ));
    }
    repository.index_mut().sort();
    repository.write_index()?;
    Ok(())
}
