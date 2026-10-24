//! `sparse-checkout`: choose which tracked paths are present in the work tree.
//!
//! `init`, `set`, `add`, `list`, `reapply` and `disable` work in cone mode (the default) and with
//! free-form patterns (`--no-cone`); the definition lives in `info/sparse-checkout`, the switches in
//! `config.worktree`, and the index carries `skip-worktree` for every path outside it.

use std::collections::BTreeSet;
use std::io::Read as _;

use anyhow::Result;
use bstr::{BStr, BString};
use rivet_core::Repository;
use rivet_core::sparse::{SparseApply, cone_patterns};

use crate::cli::GlobalArgs;

/// Arguments of `sparse-checkout`: the subcommand and everything after it.
#[derive(Debug, clap::Args)]
pub struct SparseArgs {
    /// `init`, `list`, `set`, `add`, `reapply` or `disable`, then its arguments.
    #[arg(value_name = "SUBCOMMAND", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE: &str = "usage: git sparse-checkout (init | list | set | add | reapply | disable | check-rules | clean) [<options>]\n\n";

/// The usage Git prints for a bad option of each subcommand.
fn subcommand_usage(sub: &str) -> &'static str {
    match sub {
        "init" => {
            "usage: git sparse-checkout init [--cone] [--[no-]sparse-index]\n\n    --[no-]cone           initialize the sparse-checkout in cone mode\n    --[no-]sparse-index   toggle the use of a sparse index\n\n"
        }
        "set" => {
            "usage: git sparse-checkout set [--[no-]cone] [--[no-]sparse-index] [--skip-checks] (--stdin | <patterns>)\n\n    --[no-]cone           initialize the sparse-checkout in cone mode\n    --[no-]sparse-index   toggle the use of a sparse index\n    --skip-checks         skip some sanity checks on the given paths that might give false positives\n    --stdin               read patterns from standard in\n\n"
        }
        "add" => {
            "usage: git sparse-checkout add [--skip-checks] (--stdin | <patterns>)\n\n    --skip-checks         skip some sanity checks on the given paths that might give false positives\n    --[no-]stdin          read patterns from standard in\n\n"
        }
        "reapply" => {
            "usage: git sparse-checkout reapply [--[no-]cone] [--[no-]sparse-index]\n\n    --[no-]cone           initialize the sparse-checkout in cone mode\n    --[no-]sparse-index   toggle the use of a sparse index\n\n"
        }
        "disable" => "usage: git sparse-checkout disable\n\n",
        _ => "usage: git sparse-checkout list\n\n",
    }
}

/// What the options of one subcommand asked for.
#[derive(Default)]
struct Options {
    cone: Option<bool>,
    stdin: bool,
    skip_checks: bool,
    rest: Vec<String>,
}

/// Parse the options the subcommands share; `allowed` names the ones this subcommand takes.
fn parse_options(args: &[String], allowed: &[&str]) -> std::result::Result<Options, String> {
    let mut options = Options::default();
    let mut only_paths = false;
    for arg in args {
        if only_paths || !arg.starts_with('-') || arg == "-" {
            options.rest.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => only_paths = true,
            "--cone" if allowed.contains(&"cone") => options.cone = Some(true),
            "--no-cone" if allowed.contains(&"cone") => options.cone = Some(false),
            "--no-sparse-index" if allowed.contains(&"cone") => {}
            "--stdin" if allowed.contains(&"stdin") => options.stdin = true,
            "--skip-checks" if allowed.contains(&"skip-checks") => options.skip_checks = true,
            other => {
                return Err(format!(
                    "unknown option `{}'",
                    other.trim_start_matches('-')
                ));
            }
        }
    }
    Ok(options)
}

/// Handler for `rivet sparse-checkout`.
///
/// # Caveats
///
/// `check-rules`, `clean` and the sparse index (`--sparse-index`) are not implemented. Paths given to
/// `set` and `add` are taken relative to the work tree root whatever the current directory is.
pub fn run_sparse_checkout(global: &GlobalArgs, args: &SparseArgs) -> Result<i32> {
    let Some((sub, rest)) = args.args.split_first() else {
        eprintln!("error: need a subcommand");
        eprint!("{USAGE}");
        return Ok(129);
    };
    let allowed: &[&str] = match sub.as_str() {
        "init" | "reapply" => &["cone"],
        "set" => &["cone", "stdin", "skip-checks"],
        "add" => &["stdin", "skip-checks"],
        "list" | "disable" => &[],
        "check-rules" | "clean" => {
            anyhow::bail!("sparse-checkout {sub} is not implemented");
        }
        other => {
            eprintln!("error: unknown subcommand: `{other}'");
            eprint!("{USAGE}");
            return Ok(129);
        }
    };
    let options = match parse_options(rest, allowed) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            eprint!("{}", subcommand_usage(sub));
            return Ok(129);
        }
    };
    let mut repository = super::open(global)?;
    match sub.as_str() {
        "list" => list(&repository),
        "init" => init(&mut repository, &options),
        "set" => set(&mut repository, &options, false),
        "add" => set(&mut repository, &options, true),
        "reapply" => reapply(&mut repository, &options),
        _ => disable(&mut repository),
    }
}

/// The cone switch a subcommand without a flag keeps: the current one while sparse checkout is on,
/// a cone otherwise.
fn current_cone(repository: &Repository) -> Result<bool> {
    if repository.sparse_enabled()? {
        Ok(repository.sparse_cone_setting()?.unwrap_or(false))
    } else {
        Ok(true)
    }
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("{warning}");
    }
}

/// Report what [`Repository::apply_sparse`] left alone, in Git's words.
fn report(applied: &SparseApply) {
    if !applied.unmerged.is_empty() {
        eprintln!(
            "warning: The following paths are unmerged and were left despite sparse patterns:"
        );
        for path in &applied.unmerged {
            eprintln!("\t{path}");
        }
        eprintln!();
        eprintln!(
            "After fixing the above paths, you may want to run `git sparse-checkout reapply`."
        );
    }
    if !applied.dirty.is_empty() {
        eprintln!(
            "warning: The following paths are not up to date and were left despite sparse patterns:"
        );
        for path in &applied.dirty {
            eprintln!("\t{path}");
        }
        eprintln!();
        eprintln!(
            "After fixing the above paths, you may want to run `git sparse-checkout reapply`."
        );
    }
    for path in &applied.blocked {
        eprintln!("error: Entry '{path}' not uptodate. Cannot update sparse checkout.");
    }
}

/// Apply the definition on disk and report what could not be moved.
fn apply(repository: &mut Repository) -> Result<i32> {
    let applied = match repository.sparse_checkout()? {
        Some((sparse, warnings)) => {
            print_warnings(&warnings);
            repository.apply_sparse(Some(&sparse))?
        }
        None => repository.apply_sparse(None)?,
    };
    report(&applied);
    Ok(0)
}

fn list(repository: &Repository) -> Result<i32> {
    if !repository.sparse_enabled()? {
        eprintln!("fatal: this worktree is not sparse");
        return Ok(128);
    }
    if !repository.sparse_patterns_exist() {
        eprintln!("warning: this worktree is not sparse (sparse-checkout file may not exist)");
        return Ok(0);
    }
    let Some((sparse, warnings)) = repository.sparse_checkout()? else {
        return Ok(0);
    };
    print_warnings(&warnings);
    if sparse.is_cone() {
        for dir in sparse.directories() {
            println!("{dir}");
        }
    } else {
        for line in sparse.lines() {
            println!("{line}");
        }
    }
    Ok(0)
}

fn init(repository: &mut Repository, options: &Options) -> Result<i32> {
    let cone = match options.cone {
        Some(cone) => cone,
        None => current_cone(repository)?,
    };
    repository.write_sparse_settings(true, cone, false)?;
    if !repository.sparse_patterns_exist() {
        repository.write_sparse_patterns(&cone_patterns(&BTreeSet::new()))?;
    }
    apply(repository)
}

fn reapply(repository: &mut Repository, options: &Options) -> Result<i32> {
    if !repository.sparse_enabled()? {
        eprintln!("fatal: must be in a sparse-checkout to reapply sparsity patterns");
        return Ok(128);
    }
    if let Some(cone) = options.cone {
        repository.write_sparse_settings(true, cone, false)?;
    }
    apply(repository)
}

fn disable(repository: &mut Repository) -> Result<i32> {
    let applied = repository.apply_sparse(None)?;
    repository.write_sparse_settings(false, false, true)?;
    report(&applied);
    Ok(0)
}

/// Make a path given to a cone-mode `set` or `add` a plain directory name, or say why it is not one.
fn cone_directory(repository: &Repository, arg: &str, skip_checks: bool) -> Result<String, String> {
    if !skip_checks {
        if arg.starts_with('/') {
            return Err("specify directories rather than patterns (no leading slash)".to_string());
        }
        if arg.contains(['*', '?', '[', ']', '\\']) {
            return Err("specify directories rather than patterns.  If your directory really has any of '*?[]\\' in it, pass --skip-checks".to_string());
        }
    }
    let components: Vec<&str> = arg
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let normalized = components.join("/");
    if !skip_checks
        && !normalized.is_empty()
        && repository
            .index()
            .entry(BStr::new(normalized.as_bytes()))
            .is_some()
    {
        return Err(format!(
            "'{arg}' is not a directory; to treat it as a directory anyway, rerun with --skip-checks"
        ));
    }
    Ok(normalized)
}

/// `set` replaces the definition, `add` extends it.
fn set(repository: &mut Repository, options: &Options, add: bool) -> Result<i32> {
    if add && !repository.sparse_enabled()? {
        eprintln!("fatal: no sparse-checkout to add to");
        return Ok(128);
    }
    let cone = match options.cone {
        Some(cone) => cone,
        None => current_cone(repository)?,
    };
    // Git switches the configuration on before it looks at the paths, so a refused path leaves
    // sparse checkout enabled without a pattern file.
    repository.write_sparse_settings(true, cone, false)?;
    let mut given: Vec<String> = options.rest.clone();
    if options.stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        given.extend(text.lines().map(str::to_string));
    }
    let lines: Vec<BString> = if cone {
        let mut directories: BTreeSet<BString> = BTreeSet::new();
        if add && let Some((existing, _)) = repository.sparse_checkout()? {
            directories.extend(existing.directories());
        }
        for arg in &given {
            match cone_directory(repository, arg, options.skip_checks) {
                Ok(dir) if dir.is_empty() => {}
                Ok(dir) => {
                    directories.insert(BString::from(dir));
                }
                Err(message) => {
                    eprintln!("fatal: {message}");
                    return Ok(128);
                }
            }
        }
        cone_patterns(&directories)
    } else {
        let mut lines: Vec<BString> = Vec::new();
        if add && let Some((existing, _)) = repository.sparse_checkout()? {
            lines.extend(existing.lines().iter().cloned());
        }
        lines.extend(given.iter().map(|g| BString::from(g.as_str())));
        lines
    };
    repository.write_sparse_patterns(&lines)?;
    apply(repository)
}
