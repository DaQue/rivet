//! `apply`: apply a unified diff to the work tree and/or the index.
//!
//! The patch comes from the named files or standard input. It is applied to every file or to
//! none. `--stat`, `--numstat` and `--summary` print and leave the tree alone unless `--apply` is
//! given. `--3way` and `--reject` are supported, and so is the data of a `GIT binary patch`;
//! whitespace handling is not, and is refused rather than ignored.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;

use anyhow::Result;

use super::patch::{ApplyOptions, FilePatch, apply_to_repo, parse};
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

    /// Show a diffstat instead of applying.
    #[arg(long = "stat")]
    pub stat: bool,

    /// Show a numstat instead of applying.
    #[arg(long = "numstat")]
    pub numstat: bool,

    /// Show a summary of the patch (creations, deletions, renames, mode changes).
    #[arg(long = "summary")]
    pub summary: bool,

    /// Apply the patch even when `--stat`, `--numstat` or `--summary` was given.
    #[arg(long = "apply")]
    pub apply: bool,

    /// Separate the `--numstat` fields with NUL and print names unquoted.
    #[arg(short = 'z')]
    pub nul: bool,

    /// Merge the patch into the current content when it does not apply by content.
    #[arg(short = '3', long = "3way")]
    pub three_way: bool,

    /// Leave rejected hunks in `.rej` files.
    #[arg(long = "reject")]
    pub reject: bool,
}

/// Handler for `rivet apply`.
///
/// # Exit codes
///
/// `0` applied (or checks out); `1` the patch does not apply; `128` an unreadable patch.
pub fn run_apply(global: &GlobalArgs, args: &ApplyArgs) -> Result<i32> {
    if args.three_way && args.reject {
        eprintln!("error: options '--reject' and '--3way' cannot be used together");
        return Ok(128);
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
        // `--3way` stages what it merges, like `--index`.
        index: args.index || (args.three_way && !args.cached),
        cached: args.cached,
        check: args.check,
        unidiff_zero: args.unidiff_zero,
        reject: args.reject,
        three_way: args.three_way,
    };
    let inspect = args.stat || args.numstat || args.summary;
    if inspect {
        // Git checks the patch first (with `--check` or `--apply`) and prints nothing when it
        // does not fit.
        if args.check || args.apply {
            let probe = ApplyOptions {
                check: true,
                ..options
            };
            if let Err(lines) = apply_to_repo(&mut repository, &patches, probe) {
                for line in lines {
                    eprintln!("{line}");
                }
                return Ok(1);
            }
        }
        let shown: Vec<FilePatch> = if args.reverse {
            patches.iter().rev().map(reversed).collect()
        } else {
            patches.clone()
        };
        let mut out = Vec::new();
        if args.stat {
            // Git sizes the name column from the names as the patch spells them, `-R` or not.
            let max_len = patches
                .iter()
                .map(|p| display_name(p).len())
                .max()
                .unwrap_or(0);
            write_stat(&shown, max_len, &mut out);
        }
        if args.numstat {
            write_numstat(&shown, args.nul, &mut out);
        }
        if args.summary {
            write_summary(&shown, &mut out);
        }
        std::io::stdout().write_all(&out)?;
        if !args.apply || args.check {
            return Ok(0);
        }
    }
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

/// The patch as `-R` sees it: sides, modes and counts swapped.
fn reversed(patch: &FilePatch) -> FilePatch {
    let mut out = patch.clone();
    std::mem::swap(&mut out.old_path, &mut out.new_path);
    std::mem::swap(&mut out.old_mode, &mut out.new_mode);
    std::mem::swap(&mut out.is_new, &mut out.is_delete);
    std::mem::swap(&mut out.lines_added, &mut out.lines_deleted);
    out
}

/// The name a patch is listed under: the new path, or the old one for a deletion.
fn display_name(patch: &FilePatch) -> Vec<u8> {
    let name = patch.new_path.as_ref().or(patch.old_path.as_ref());
    rivet_core::ops::quote_path(name.map_or(&b""[..], |n| n.as_slice()), false).to_vec()
}

/// `--stat`, in the layout of git's `apply.c` (not the one of `diff --stat`).
fn write_stat(patches: &[FilePatch], max_len: usize, out: &mut Vec<u8>) {
    let names: Vec<Vec<u8>> = patches.iter().map(display_name).collect();
    let max_change = patches
        .iter()
        .map(|p| p.lines_added + p.lines_deleted)
        .max()
        .unwrap_or(0);
    let (mut files, mut added, mut deleted) = (0usize, 0usize, 0usize);
    for (patch, name) in patches.iter().zip(&names) {
        let width = max_len.min(50);
        let mut name = name.clone();
        if name.len() > width {
            let from = (name.len() + 3).saturating_sub(width).min(name.len());
            let cut = name[from..]
                .iter()
                .position(|b| *b == b'/')
                .map_or(from, |at| from + at);
            let mut shown = b"...".to_vec();
            shown.extend_from_slice(&name[cut..]);
            name = shown;
        }
        out.push(b' ');
        out.extend_from_slice(&name);
        out.extend(std::iter::repeat_n(b' ', width.saturating_sub(name.len())));
        files += 1;
        if patch.binary {
            out.extend_from_slice(b" |  Bin\n");
            continue;
        }
        let (mut plus, mut minus) = (patch.lines_added, patch.lines_deleted);
        let scale = if width + max_change > 70 {
            70 - width
        } else {
            max_change
        };
        let round = |n: usize| (n * scale + max_change / 2).checked_div(max_change);
        if let (Some(total), Some(share)) = (round(plus + minus), round(plus)) {
            plus = share;
            minus = total - share;
        }
        added += patch.lines_added;
        deleted += patch.lines_deleted;
        out.extend_from_slice(
            format!(" |{:>5} ", patch.lines_added + patch.lines_deleted).as_bytes(),
        );
        out.extend(std::iter::repeat_n(b'+', plus));
        out.extend(std::iter::repeat_n(b'-', minus));
        out.push(b'\n');
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut line = format!(" {files} file{} changed", plural(files));
    if added > 0 || deleted == 0 {
        line.push_str(&format!(", {added} insertion{}(+)", plural(added)));
    }
    if deleted > 0 || added == 0 {
        line.push_str(&format!(", {deleted} deletion{}(-)", plural(deleted)));
    }
    line.push('\n');
    out.extend_from_slice(line.as_bytes());
}

/// `--numstat`: `added<TAB>deleted<TAB>name`, `-` for binary files; `-z` ends the name with NUL
/// and leaves it unquoted.
fn write_numstat(patches: &[FilePatch], nul: bool, out: &mut Vec<u8>) {
    for patch in patches {
        if patch.binary {
            out.extend_from_slice(b"-\t-\t");
        } else {
            out.extend_from_slice(
                format!("{}\t{}\t", patch.lines_added, patch.lines_deleted).as_bytes(),
            );
        }
        if nul {
            let name = patch.new_path.as_ref().or(patch.old_path.as_ref());
            out.extend_from_slice(name.map_or(&b""[..], |n| n.as_slice()));
            out.push(0);
        } else {
            out.extend_from_slice(&display_name(patch));
            out.push(b'\n');
        }
    }
}

/// `--summary`: creations, deletions, renames and copies, mode changes.
fn write_summary(patches: &[FilePatch], out: &mut Vec<u8>) {
    let quote = |name: Option<&Vec<u8>>| name.cloned().unwrap_or_default();
    for patch in patches {
        let line_for = |verb: &str, mode: Option<u32>, name: Vec<u8>| {
            let mut line = format!(" {verb} mode {:06o} ", mode.unwrap_or(0o100644)).into_bytes();
            line.extend_from_slice(&name);
            line.push(b'\n');
            line
        };
        if patch.is_new {
            out.extend(line_for(
                "create",
                patch.new_mode,
                quote(patch.new_path.as_ref()),
            ));
        } else if patch.is_delete {
            out.extend(line_for(
                "delete",
                patch.old_mode,
                quote(patch.old_path.as_ref()),
            ));
        } else if patch.rename || patch.copy {
            let verb = if patch.rename { "rename" } else { "copy" };
            out.extend_from_slice(format!(" {verb} ").as_bytes());
            out.extend(pretty_rename(
                patch.old_path.as_deref().unwrap_or_default(),
                patch.new_path.as_deref().unwrap_or_default(),
            ));
            out.extend_from_slice(format!(" ({}%)\n", patch.similarity.unwrap_or(0)).as_bytes());
        }
        if !patch.is_new
            && !patch.is_delete
            && let (Some(old), Some(new)) = (patch.old_mode, patch.new_mode)
            && old != new
        {
            out.extend_from_slice(format!(" mode change {old:06o} => {new:06o}").as_bytes());
            // A rename or copy line already named the file.
            if !(patch.rename || patch.copy) {
                out.push(b' ');
                out.extend(quote(patch.new_path.as_ref()));
            }
            out.push(b'\n');
        }
    }
}

/// `dir/{old => new}`: the leading directories both names share factored out (git's `apply.c`
/// does not factor out a common tail).
fn pretty_rename(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut common = 0;
    loop {
        let (rest_old, rest_new) = (&old[common..], &new[common..]);
        let (Some(a), Some(b)) = (
            rest_old.iter().position(|c| *c == b'/'),
            rest_new.iter().position(|c| *c == b'/'),
        ) else {
            break;
        };
        if a != b || rest_old[..a] != rest_new[..b] {
            break;
        }
        common += a + 1;
    }
    let mut out = Vec::new();
    if common > 0 {
        out.extend_from_slice(&old[..common]);
        out.push(b'{');
    }
    out.extend_from_slice(&old[common..]);
    out.extend_from_slice(b" => ");
    out.extend_from_slice(&new[common..]);
    if common > 0 {
        out.push(b'}');
    }
    out
}
