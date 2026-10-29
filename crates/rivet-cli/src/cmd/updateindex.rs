//! `update-index`: the index-format options only (`--index-version`, `--show-index-version`,
//! `--[no-]split-index`). Everything else is refused by name.

use anyhow::Result;
use rivet_core::index::{IndexVersion, SplitMode};

use crate::cli::GlobalArgs;

const USAGE: &str = include_str!("updateindex_usage.txt");

/// Arguments of `update-index`, in git's own grammar.
#[derive(Debug, clap::Args)]
pub struct UpdateIndexArgs {
    /// `[--index-version <n>] [--show-index-version] [--split-index | --no-split-index]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

/// Every long option of git's `update-index` that this command does not implement.
const REFUSED: [&str; 28] = [
    "add",
    "replace",
    "remove",
    "unmerged",
    "refresh",
    "really-refresh",
    "cacheinfo",
    "chmod",
    "assume-unchanged",
    "no-assume-unchanged",
    "skip-worktree",
    "no-skip-worktree",
    "info-only",
    "force-remove",
    "stdin",
    "index-info",
    "unresolve",
    "again",
    "ignore-missing",
    "verbose",
    "clear-resolve-undo",
    "untracked-cache",
    "no-untracked-cache",
    "test-untracked-cache",
    "force-untracked-cache",
    "force-write-index",
    "fsmonitor",
    "fsmonitor-valid",
];

/// `update-index` handler.
pub fn run_update_index(global: &GlobalArgs, args: &UpdateIndexArgs) -> Result<i32> {
    let mut show = false;
    let mut version: Option<u32> = None;
    let mut split: Option<bool> = None;
    let mut words = args.args.iter();
    while let Some(word) = words.next() {
        if word == "-h" {
            print!("{USAGE}");
            return Ok(0);
        }
        let Some(long) = word.strip_prefix("--") else {
            if word == "-q" || word == "-g" || word == "-z" {
                if word == "-q" {
                    continue;
                }
                eprintln!("fatal: update-index: {word} is not implemented");
                return Ok(128);
            }
            if word.len() > 1 && word.starts_with('-') {
                eprintln!("error: unknown switch `{}'", &word[1..2]);
                eprint!("{USAGE}");
                return Ok(129);
            }
            eprintln!("fatal: update-index: paths are not implemented");
            return Ok(128);
        };
        let (name, inline) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        };
        match name {
            "show-index-version" => show = true,
            "no-show-index-version" => show = false,
            "split-index" => split = Some(true),
            "no-split-index" => split = Some(false),
            "no-index-version" => version = None,
            "index-version" => {
                let value = match inline {
                    Some(value) => Some(value.to_string()),
                    None => words.next().cloned(),
                };
                let Some(value) = value else {
                    eprintln!("error: option `index-version' requires a value");
                    return Ok(129);
                };
                let Ok(number) = value.parse::<u32>() else {
                    eprintln!(
                        "error: option `index-version' expects an integer value with an optional k/m/g suffix"
                    );
                    return Ok(129);
                };
                if !(2..=4).contains(&number) {
                    eprintln!("fatal: index-version {number} not in range: 2..4");
                    return Ok(128);
                }
                version = Some(number);
            }
            other => {
                let base = other.strip_prefix("no-").unwrap_or(other);
                if REFUSED.contains(&other) || REFUSED.contains(&base) {
                    eprintln!("fatal: update-index: --{other} is not implemented");
                    return Ok(128);
                }
                eprintln!("error: unknown option '{other}'");
                eprint!("{USAGE}");
                return Ok(129);
            }
        }
    }

    let repository = super::open(global)?;
    let path = repository.git_dir().join("index");
    if show {
        // The version the file had when it was read; a missing file reports 0.
        let shown = if path.exists() {
            repository.index().version().as_u32()
        } else {
            0
        };
        println!("{shown}");
    }
    if version.is_none() && split.is_none() {
        return Ok(0);
    }
    let configured = repository
        .config_value(bstr::BStr::new("core.splitindex"))
        .ok()
        .flatten()
        .and_then(|v| rivet_core::config::parse_bool(bstr::BStr::new(&v[..])).ok());
    match (split, configured) {
        (Some(true), Some(false)) => eprintln!(
            "warning: core.splitIndex is set to false; remove or change it, if you really want to enable split index"
        ),
        (Some(false), Some(true)) => eprintln!(
            "warning: core.splitIndex is set to true; remove or change it, if you really want to disable split index"
        ),
        _ => {}
    }
    let mut index = repository.index().clone();
    if let Some(number) = version {
        index.set_version(IndexVersion::from_u32(number)?);
    }
    // `core.splitIndex=false` removes a split index that was already there at write time; one
    // this very command creates survives.
    let already_split = index.extensions().iter().any(|e| e.is_split_index_link());
    let mode = match split {
        Some(true) if configured == Some(false) && already_split => SplitMode::Disable,
        Some(true) => SplitMode::Enable,
        Some(false) => SplitMode::Disable,
        None => SplitMode::Keep,
    };
    index.write_with(&path, repository.format(), mode, 20)?;
    Ok(0)
}
