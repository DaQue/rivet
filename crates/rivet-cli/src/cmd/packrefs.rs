//! `pack-refs`: move loose refs into `packed-refs`.

use std::path::PathBuf;

use anyhow::Result;
use rivet_core::{ObjectKind, PackedRef, PackedRefs, RefTarget};

use crate::cli::GlobalArgs;

/// Arguments to `rivet pack-refs`.
#[derive(Debug, clap::Args)]
pub struct PackRefsArgs {
    /// Pack every ref, not only tags and the refs that are already packed.
    #[arg(long = "all", overrides_with = "no_all")]
    pub all: bool,

    /// Pack only tags (the default).
    #[arg(long = "no-all", overrides_with = "all")]
    pub no_all: bool,

    /// Keep the loose ref files after packing them.
    #[arg(long = "no-prune")]
    pub no_prune: bool,

    /// Accepted for the sake of `gc --auto`; Rivet always packs when asked.
    #[arg(long = "auto")]
    pub auto: bool,

    /// Also pack the refs matching this pattern.
    #[arg(long = "include", value_name = "PATTERN")]
    pub include: Vec<String>,

    /// Leave the refs matching this pattern alone.
    #[arg(long = "exclude", value_name = "PATTERN")]
    pub exclude: Vec<String>,
}

/// Handler for `rivet pack-refs`.
///
/// Packs tags (or, with `--all`, every ref that is not symbolic or broken) into
/// `packed-refs`, with peeled values for tag objects, and then removes the loose files of the refs
/// it packed unless `--no-prune` was given. Refs already in `packed-refs` stay there.
///
/// # Exit codes
///
/// `0`; `128` outside a repository.
pub fn run_pack_refs(global: &GlobalArgs, args: &PackRefsArgs) -> Result<i32> {
    if args.auto {
        anyhow::bail!("pack-refs: --auto is not implemented");
    }
    let repository = super::open(global)?;
    let git_dir = repository.git_dir().to_path_buf();
    let odb = repository.odb();

    let mut packed = PackedRefs::new();
    packed.set_traits(vec![
        "peeled".into(),
        "fully-peeled".into(),
        "sorted".into(),
    ]);
    for entry in repository.refs().packed().iter() {
        packed.insert(entry.clone());
    }

    let matches = |patterns: &[String], name: &str| {
        patterns
            .iter()
            .any(|p| super::branching::glob_match(p.as_bytes(), name.as_bytes()))
    };
    let mut pruned: Vec<PathBuf> = Vec::new();
    for (name, target) in repository.refs().list(b"refs/")? {
        let RefTarget::Direct(oid) = target else {
            continue;
        };
        let text = name.to_string();
        // Per-worktree refs stay where they are.
        if text.starts_with("refs/bisect/")
            || text.starts_with("refs/worktree/")
            || text.starts_with("refs/rewritten/")
        {
            continue;
        }
        if !odb.contains(&oid) || matches(&args.exclude, &text) {
            continue;
        }
        let wanted = args.all && !args.no_all
            || text.starts_with("refs/tags/")
            || matches(&args.include, &text);
        if !wanted {
            continue;
        }
        let peeled = match odb.read_header(&oid) {
            Ok((ObjectKind::Tag, _)) => Some(repository.refs().peel_oid(oid, odb)?),
            _ => None,
        };
        let path = name.file_path(&git_dir);
        if path.is_file() {
            pruned.push(path);
        }
        packed.insert(PackedRef {
            name,
            target: oid,
            peeled,
        });
    }
    packed.sort();
    packed.write(&git_dir, repository.format())?;

    if !args.no_prune {
        for path in pruned {
            let _ = std::fs::remove_file(&path);
            let mut dir = path.parent();
            while let Some(d) = dir {
                // `refs/` and its first level stay even when empty.
                if d.parent() == Some(git_dir.join("refs").as_path())
                    || d == git_dir.join("refs")
                    || std::fs::remove_dir(d).is_err()
                {
                    break;
                }
                dir = d.parent();
            }
        }
    }
    Ok(0)
}
