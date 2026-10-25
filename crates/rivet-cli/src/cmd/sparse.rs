//! `sparse-checkout`: choose which tracked paths are present in the work tree.
//!
//! `init`, `set`, `add`, `list`, `reapply` and `disable` work in cone mode (the default) and with
//! free-form patterns (`--no-cone`); the definition lives in `info/sparse-checkout`, the switches in
//! `config.worktree`, and the index carries `skip-worktree` for every path outside it.

use std::collections::BTreeSet;
use std::io::Read as _;

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice as _};
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
    sparse_index: bool,
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
            "--sparse-index" if allowed.contains(&"cone") => options.sparse_index = true,
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
/// `clean` and the sparse index (`--sparse-index`) are not implemented. In cone mode, paths given
/// on the command line to `set` and `add` are relative to the current directory and those read from
/// `--stdin` to the work tree root; free-form patterns need the top-level directory.
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
        "check-rules" => return check_rules(global, rest),
        "clean" => {
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
    if options.sparse_index {
        // Refused before anything is touched: a full index under `index.sparse=true` would not be
        // the index git writes.
        anyhow::bail!("sparse-checkout {sub} --sparse-index is not implemented");
    }
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

const CHECK_RULES_USAGE: &str = "usage: git sparse-checkout check-rules [-z] [--skip-checks][--[no-]cone] [--rules-file <file>]\n\n    -z                    terminate input and output files by a NUL character\n    --[no-]cone           when used with --rules-file interpret patterns as cone mode patterns\n    --[no-]rules-file <file>\n                          use patterns in <file> instead of the current ones.\n\n";

/// The bytes a C-style quoted string (`"…"`, with `\n`, `\"`, `\ooo` escapes) stands for.
fn unquote_c_style(text: &[u8]) -> Option<Vec<u8>> {
    let inner = text.strip_prefix(b"\"")?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < inner.len() {
        match inner[i] {
            b'"' => return (i + 1 == inner.len()).then_some(out),
            b'\\' => {
                i += 1;
                let escaped = *inner.get(i)?;
                match escaped {
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'v' => out.push(11),
                    b'\\' | b'"' => out.push(escaped),
                    b'0'..=b'3' => {
                        let digits = inner.get(i..i + 3)?;
                        if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                            return None;
                        }
                        let value = digits.iter().fold(0u32, |n, d| n * 8 + u32::from(d - b'0'));
                        out.push(u8::try_from(value).ok()?);
                        i += 2;
                    }
                    _ => return None,
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    None
}

/// `check-rules`: read paths from standard input and print those the definition includes.
fn check_rules(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    use std::io::Write as _;
    let mut nul = false;
    let mut cone = true;
    let mut rules_file: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-z" => nul = true,
            "--cone" => cone = true,
            "--no-cone" => cone = false,
            "--no-rules-file" => rules_file = None,
            "--rules-file" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("error: option `rules-file' requires a value");
                    eprint!("{CHECK_RULES_USAGE}");
                    return Ok(129);
                };
                rules_file = Some(value.clone());
            }
            _ if arg.starts_with("--rules-file=") => {
                rules_file = Some(arg["--rules-file=".len()..].to_string());
            }
            _ if arg.starts_with('-') && arg != "-" => {
                eprintln!("error: unknown option `{}'", arg.trim_start_matches('-'));
                eprint!("{CHECK_RULES_USAGE}");
                return Ok(129);
            }
            _ => {}
        }
        index += 1;
    }
    let repository = super::open(global)?;
    let (data, cone) = match &rules_file {
        Some(file) => match std::fs::read(file) {
            Ok(data) => (data, cone),
            Err(error) => {
                let reason = match error.kind() {
                    std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
                    _ => error.to_string(),
                };
                eprintln!("fatal: could not open '{file}' for reading: {reason}");
                return Ok(128);
            }
        },
        None => {
            let Ok(data) = std::fs::read(repository.git_dir().join("info").join("sparse-checkout"))
            else {
                eprintln!("fatal: unable to load existing sparse-checkout patterns");
                return Ok(128);
            };
            (data, repository.sparse_cone_setting()?.unwrap_or(true))
        }
    };
    let (mut sparse, mut warnings) = rivet_core::sparse::SparseCheckout::parse(cone, &data)?;
    if cone && rules_file.is_some() && !warnings.is_empty() {
        // Git reads the lines of a cone rules file it does not understand as nothing, quietly,
        // instead of falling back to free-form patterns.
        let mut kept = Vec::new();
        for line in data.lines() {
            let (_, line_warnings) = rivet_core::sparse::SparseCheckout::parse(true, line)?;
            if line_warnings.is_empty() {
                kept.extend_from_slice(line);
                kept.push(b'\n');
            }
        }
        (sparse, warnings) = rivet_core::sparse::SparseCheckout::parse(true, &kept)?;
    }
    print_warnings(&warnings);
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let separator = if nul { b'\0' } else { b'\n' };
    let mut records: Vec<&[u8]> = input.split(|b| *b == separator).collect();
    if records.last().is_some_and(|last| last.is_empty()) {
        records.pop();
    }
    let mut out = std::io::stdout().lock();
    for record in records {
        let path = if nul || !record.starts_with(b"\"") {
            record.to_vec()
        } else if let Some(path) = unquote_c_style(record) {
            path
        } else {
            out.flush()?;
            eprintln!(
                "fatal: unable to unquote C-style string '{}'",
                String::from_utf8_lossy(record)
            );
            return Ok(128);
        };
        // Git never counts an absolute path as part of a cone.
        if (sparse.is_cone() && path.starts_with(b"/")) || !sparse.includes(BStr::new(&path)) {
            continue;
        }
        if nul {
            out.write_all(&path)?;
            out.write_all(b"\0")?;
        } else {
            out.write_all(&rivet_core::ops::quote_path(&path, false))?;
            out.write_all(b"\n")?;
        }
    }
    out.flush()?;
    Ok(0)
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
    for dir in &applied.untracked_dirs {
        eprintln!(
            "warning: directory '{dir}/' contains untracked files, but is not in the sparse-checkout cone"
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

/// The current directory below the work tree root, without a trailing slash (empty at the root).
fn cwd_prefix(repository: &Repository) -> Result<String> {
    let Some(root) = repository
        .worktree()?
        .root()
        .map(std::path::Path::to_path_buf)
    else {
        return Ok(String::new());
    };
    let here = std::env::current_dir()?;
    Ok(here
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default())
}

/// A path argument made relative to the work tree root, as git's `prefix_path` does when it runs in
/// a subdirectory; `..` is resolved and a path that leaves the work tree is refused.
fn with_prefix(repository: &Repository, prefix: &str, arg: &str) -> Result<String, String> {
    if prefix.is_empty() {
        return Ok(arg.to_string());
    }
    let root = repository
        .worktree()
        .ok()
        .and_then(|w| w.root().map(std::path::Path::to_path_buf))
        .unwrap_or_default();
    let outside = || format!("'{arg}' is outside repository at '{}'", root.display());
    let joined = if let Some(absolute) = arg.strip_prefix('/') {
        let root_text = root.to_string_lossy();
        let root_text = root_text.trim_start_matches('/');
        let inside = absolute
            .strip_prefix(root_text)
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
            .ok_or_else(outside)?;
        inside.to_string()
    } else {
        format!("{prefix}/{arg}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for component in joined.split('/').filter(|c| !c.is_empty() && *c != ".") {
        if component == ".." {
            parts.pop().ok_or_else(outside)?;
        } else {
            parts.push(component);
        }
    }
    Ok(parts.join("/"))
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
    let prefix = cwd_prefix(repository)?;
    // Git switches the configuration on before it looks at the paths, so a refused path leaves
    // sparse checkout enabled without a pattern file (and a refused `--no-cone` in cone-less mode).
    repository.write_sparse_settings(true, cone, false)?;
    if !cone && !prefix.is_empty() {
        eprintln!("fatal: please run from the toplevel directory in non-cone mode");
        return Ok(128);
    }
    let mut given: Vec<String> = Vec::new();
    for arg in &options.rest {
        match with_prefix(repository, &prefix, arg) {
            Ok(path) => given.push(path),
            Err(message) => {
                eprintln!("fatal: {message}");
                return Ok(128);
            }
        }
    }
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
