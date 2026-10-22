//! `apply`: apply a unified diff to the work tree and/or the index.
//!
//! The patch comes from the named files or standard input. It is applied to every file or to
//! none. `--stat`, `--numstat`, `--summary`, `--3way`, `--reject`, whitespace handling and binary
//! patches are not implemented and are refused rather than ignored.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::Result;

use super::patch::{ApplyOptions, apply_to_repo, parse};
use crate::cli::GlobalArgs;

/// Arguments of `apply`.
#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    /// Patch files (default: standard input).
    #[arg(value_name = "PATCH")]
    pub patches: Vec<PathBuf>,

    /// Apply to the index and the work tree.
    #[arg(long = "index")]
    pub index: bool,

    /// Apply to the index only.
    #[arg(long = "cached")]
    pub cached: bool,

    /// Check that the patch applies; change nothing.
    #[arg(long = "check")]
    pub check: bool,

    /// Apply the patch in reverse.
    #[arg(short = 'R', long = "reverse")]
    pub reverse: bool,

    /// Remove this many leading path components (default 1).
    #[arg(short = 'p', value_name = "N", default_value_t = 1)]
    pub strip: usize,

    /// Accept patches with zero lines of context.
    #[arg(long = "unidiff-zero")]
    pub unidiff_zero: bool,

    /// Show a diffstat instead of applying. Not implemented.
    #[arg(long = "stat")]
    pub stat: bool,

    /// Show a numstat instead of applying. Not implemented.
    #[arg(long = "numstat")]
    pub numstat: bool,

    /// Show a summary of the patch. Not implemented.
    #[arg(long = "summary")]
    pub summary: bool,

    /// Fall back on a three-way merge. Not implemented.
    #[arg(short = '3', long = "3way")]
    pub three_way: bool,

    /// Leave rejected hunks in `.rej` files. Not implemented.
    #[arg(long = "reject")]
    pub reject: bool,
}

/// Handler for `rivet apply`.
///
/// # Exit codes
///
/// `0` applied (or checks out); `1` the patch does not apply; `128` an unreadable patch.
pub fn run_apply(global: &GlobalArgs, args: &ApplyArgs) -> Result<i32> {
    if args.stat || args.numstat || args.summary || args.three_way || args.reject {
        anyhow::bail!(
            "apply: --stat, --numstat, --summary, --3way and --reject are not implemented"
        );
    }
    let mut repository = super::open(global)?;
    let mut text = Vec::new();
    if args.patches.is_empty() {
        std::io::stdin().read_to_end(&mut text)?;
    } else {
        for path in &args.patches {
            if path.as_os_str() == "-" {
                std::io::stdin().read_to_end(&mut text)?;
            } else {
                match std::fs::read(path) {
                    Ok(data) => text.extend_from_slice(&data),
                    Err(error) => {
                        let reason = if error.kind() == std::io::ErrorKind::NotFound {
                            "No such file or directory".to_string()
                        } else {
                            error.to_string()
                        };
                        eprintln!("error: can't open patch '{}': {reason}", path.display());
                        return Ok(128);
                    }
                }
            }
            text.push(b'\n');
        }
    }
    let patches = match parse(&text, args.strip) {
        Ok(patches) => patches,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(128);
        }
    };
    if patches.is_empty() {
        eprintln!("error: No valid patches in input (allow with \"--allow-empty\")");
        return Ok(128);
    }
    let options = ApplyOptions {
        reverse: args.reverse,
        index: args.index,
        cached: args.cached,
        check: args.check,
        unidiff_zero: args.unidiff_zero,
    };
    match apply_to_repo(&mut repository, &patches, options) {
        Ok(warnings) => {
            for line in warnings {
                eprintln!("{line}");
            }
            Ok(0)
        }
        Err(lines) => {
            for line in lines {
                eprintln!("{line}");
            }
            Ok(1)
        }
    }
}
