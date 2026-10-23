//! `cherry`: find commits not yet applied upstream, by patch equality.

use std::collections::HashSet;

use anyhow::Result;
use rivet_core::ObjectId;
use rivet_core::revwalk::{SortOrder, WalkOptions};

use super::rebase::{patch_id, subject_of, tracked_upstream};
use crate::cli::GlobalArgs;

/// Arguments to `rivet cherry`.
#[derive(Debug, clap::Args)]
pub struct CherryArgs {
    /// Show the subject next to each commit.
    #[arg(short = 'v', long = "verbose", overrides_with = "no_verbose")]
    pub verbose: bool,

    /// Show only the markers and ids (the default).
    #[arg(long = "no-verbose", overrides_with = "verbose")]
    pub no_verbose: bool,

    /// `[<upstream> [<head> [<limit>]]]`.
    #[arg(value_name = "REV")]
    pub revs: Vec<String>,
}

const USAGE: &str = "usage: git cherry [-v] [<upstream> [<head> [<limit>]]]\n\n    --[no-]abbrev[=<n>]   use <n> digits to display object names\n    -v, --[no-]verbose    be verbose\n\n";

/// Handler for `rivet cherry`.
///
/// Lists the non-merge commits of `<upstream>..<head>` (optionally cut off at `<limit>`), oldest
/// first, with `-` when an equivalent patch is in `<head>..<upstream>` and `+` when not.
pub fn run_cherry(global: &GlobalArgs, args: &CherryArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let head_state = repository.head()?;
    let (upstream, head, limit) = match args.revs.as_slice() {
        [u] => (u.clone(), "HEAD".to_string(), None),
        [u, h] => (u.clone(), h.clone(), None),
        [u, h, l] => (u.clone(), h.clone(), Some(l.clone())),
        _ => {
            let tracked = if args.revs.is_empty() {
                tracked_upstream(&repository, &head_state)?
            } else {
                None
            };
            match tracked {
                Some(upstream) => (upstream, "HEAD".to_string(), None),
                None => {
                    eprintln!(
                        "Could not find a tracked remote branch, please specify <upstream> manually."
                    );
                    eprint!("{USAGE}");
                    return Ok(129);
                }
            }
        }
    };
    let resolve = |spec: &str| -> Result<Option<ObjectId>> {
        match super::resolve_commit(&repository, spec) {
            Ok(oid) => Ok(Some(oid)),
            Err(_) => {
                eprintln!("fatal: unknown commit {spec}");
                Ok(None)
            }
        }
    };
    let Some(upstream_oid) = resolve(&upstream)? else {
        return Ok(128);
    };
    let Some(head_oid) = resolve(&head)? else {
        return Ok(128);
    };
    let limit_oid = match &limit {
        Some(spec) => match resolve(spec)? {
            Some(oid) => Some(oid),
            None => return Ok(128),
        },
        None => None,
    };

    let odb = repository.odb();
    let mut theirs_options = WalkOptions::new();
    theirs_options.exclude(head_oid);
    let upstream_only = rivet_core::revwalk::walk_oids(odb, &[upstream_oid], &theirs_options)?;
    let mut upstream_ids: HashSet<Vec<u8>> = HashSet::new();
    for oid in &upstream_only {
        if repository.commit_parents(*oid)?.len() <= 1 {
            upstream_ids.insert(patch_id(&repository, oid)?);
        }
    }

    let mut options = WalkOptions::new();
    options.exclude(upstream_oid);
    if let Some(limit) = limit_oid {
        options.exclude(limit);
    }
    options.sort(SortOrder::Topo);
    let mut range = rivet_core::revwalk::walk_oids(odb, &[head_oid], &options)?;
    range.reverse();
    for oid in range {
        if repository.commit_parents(oid)?.len() > 1 {
            continue;
        }
        let mark = if upstream_ids.contains(&patch_id(&repository, &oid)?) {
            '-'
        } else {
            '+'
        };
        if args.verbose {
            println!("{mark} {oid} {}", subject_of(&repository, &oid)?);
        } else {
            println!("{mark} {oid}");
        }
    }
    Ok(0)
}
