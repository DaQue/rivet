//! `grep`: search tracked files in the work tree, the index (`--cached`) or a tree.
//!
//! Patterns are POSIX basic regular expressions by default, `-E` extended ones, `-F` fixed
//! strings. `-P` (Perl), `--and`/`--not` boolean patterns are not
//! implemented and are refused at the command line. `--threads` is validated and ignored.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use bstr::{BStr, BString};
use rivet_core::{ObjectId, ObjectKind, Repository};

use super::regex::Regex;
use crate::cli::GlobalArgs;

/// Arguments of `grep`.
#[derive(Debug, clap::Args)]
pub struct GrepArgs {
    /// The pattern (unless `-e` is used), then revisions, then (after `--`) paths.
    #[arg(value_name = "ARG")]
    pub args: Vec<String>,

    /// Limit the search to these paths.
    #[arg(last = true, value_name = "PATHSPEC")]
    pub paths: Vec<String>,

    /// A pattern; may be given several times (a line matching any of them is a match).
    #[arg(short = 'e', value_name = "PATTERN")]
    pub patterns: Vec<String>,

    /// Search the index instead of the work tree.
    #[arg(long = "cached")]
    pub cached: bool,

    /// Also search the untracked files that are not ignored.
    #[arg(long = "untracked")]
    pub untracked: bool,

    /// Ignore case differences.
    #[arg(short = 'i', long = "ignore-case")]
    pub ignore_case: bool,

    /// Match only whole words.
    #[arg(short = 'w', long = "word-regexp")]
    pub word: bool,

    /// Select the lines that do not match (`-v`, which the grammar's global flag provides).
    #[arg(long = "invert-match")]
    pub invert: bool,

    /// Fixed strings, not regular expressions.
    #[arg(short = 'F', long = "fixed-strings")]
    pub fixed: bool,

    /// Extended regular expressions.
    #[arg(short = 'E', long = "extended-regexp")]
    pub extended: bool,

    /// Basic regular expressions (the default).
    #[arg(short = 'G', long = "basic-regexp")]
    pub basic: bool,

    /// Prefix each line with its line number.
    #[arg(short = 'n', long = "line-number")]
    pub line_number: bool,

    /// Print only the names of files with a match.
    #[arg(short = 'l', long = "files-with-matches", alias = "name-only")]
    pub files_with_matches: bool,

    /// Print only the names of files without a match.
    #[arg(short = 'L', long = "files-without-match")]
    pub files_without_match: bool,

    /// Print the number of matching lines per file.
    #[arg(short = 'c', long = "count")]
    pub count: bool,

    /// Do not print the file name.
    #[arg(short = 'h')]
    pub no_filename: bool,

    /// Show this help (`-h` means `--no-filename` here, as in Git).
    #[arg(long = "help", action = clap::ArgAction::Help)]
    pub help: Option<bool>,

    /// Print the file name (the default).
    #[arg(short = 'H')]
    pub with_filename: bool,

    /// Treat binary files as text.
    #[arg(short = 'a', long = "text")]
    pub text: bool,

    /// Skip binary files.
    #[arg(short = 'I')]
    pub skip_binary: bool,

    /// Print only the matching parts of a line, one per output line.
    #[arg(
        short = 'o',
        long = "only-matching",
        overrides_with = "no_only_matching"
    )]
    pub only_matching: bool,

    /// Print whole lines again after `-o`.
    #[arg(long = "no-only-matching", overrides_with = "only_matching")]
    pub no_only_matching: bool,

    /// Print the file name once above its matches instead of on every line.
    #[arg(long = "heading", overrides_with = "no_heading")]
    pub heading: bool,

    /// Print the file name on every line (the default).
    #[arg(long = "no-heading", overrides_with = "heading")]
    pub no_heading: bool,

    /// Print NUL instead of the `:`/`-` after a file name and a line number, and instead of the
    /// newline after the names of `-l`/`-L`.
    #[arg(short = 'z', long = "null", overrides_with = "no_null")]
    pub null: bool,

    /// Print the usual separators again after `-z`.
    #[arg(long = "no-null", overrides_with = "null")]
    pub no_null: bool,

    /// Print an empty line between the output of different files.
    #[arg(long = "break", overrides_with = "no_break")]
    pub file_break: bool,

    /// Print no empty line between files (the default).
    #[arg(long = "no-break", overrides_with = "file_break")]
    pub no_break: bool,

    /// Lines of context after a match.
    #[arg(short = 'A', long = "after-context", value_name = "N")]
    pub after: Option<usize>,

    /// Lines of context before a match.
    #[arg(short = 'B', long = "before-context", value_name = "N")]
    pub before: Option<usize>,

    /// Lines of context around a match.
    #[arg(short = 'C', long = "context", value_name = "N")]
    pub context: Option<usize>,

    /// Worker threads: validated like git's, but the search is always sequential.
    #[arg(
        long = "threads",
        value_name = "N",
        allow_hyphen_values = true,
        num_args = 0..=1,
        default_missing_value = "\u{0}missing",
        overrides_with = "no_threads"
    )]
    pub threads: Option<String>,

    /// Forget an earlier `--threads`.
    #[arg(long = "no-threads", overrides_with = "threads")]
    pub no_threads: bool,
}

/// Parse an integer the way git's option parser does: an optional sign, digits and an optional
/// `k`/`m`/`g` suffix.
fn parse_git_int(text: &str) -> Option<i64> {
    let (digits, factor) = match text.chars().last()? {
        'k' | 'K' => (&text[..text.len() - 1], 1i64 << 10),
        'm' | 'M' => (&text[..text.len() - 1], 1 << 20),
        'g' | 'G' => (&text[..text.len() - 1], 1 << 30),
        _ => (text, 1),
    };
    digits.parse::<i64>().ok()?.checked_mul(factor)
}

struct Source {
    /// The name as printed: `path`, or `rev:path` when searching a tree.
    shown: Vec<u8>,
    data: Vec<u8>,
}

/// Handler for `rivet grep`.
///
/// # Exit codes
///
/// `0` when something matched, `1` when nothing did, `128` for a bad pattern or revision.
pub fn run_grep(global: &GlobalArgs, args: &GrepArgs) -> Result<i32> {
    let threads = match args.threads.as_deref().map(|t| (t, parse_git_int(t))) {
        Some(("\u{0}missing", _)) => {
            eprintln!("error: option `threads' requires a value");
            return Ok(129);
        }
        Some(("", _)) => {
            eprintln!("error: option `threads' expects a numerical value");
            return Ok(129);
        }
        Some((_, None)) => {
            eprintln!(
                "error: option `threads' expects an integer value with an optional k/m/g suffix"
            );
            return Ok(129);
        }
        Some((_, Some(n))) => Some(n),
        None => None,
    };
    let mut repository = super::open(global)?;
    // `-v` and `-q` are the grammar's global flags; here they mean invert and quiet.
    let invert = args.invert || global.verbose;
    let quiet = global.quiet;
    let mut positional: Vec<String> = args.args.clone();
    let mut after_dashes: Vec<String> = args.paths.clone();
    if positional.is_empty() && args.patterns.is_empty() && !after_dashes.is_empty() {
        // `grep -- <pattern> <paths>`: the dashes only end the options.
        positional.push(after_dashes.remove(0));
    }
    let mut patterns: Vec<String> = args.patterns.clone();
    if patterns.is_empty() {
        if positional.is_empty() {
            eprintln!("fatal: no pattern given");
            return Ok(128);
        }
        patterns.push(positional.remove(0));
    }
    if let Some(n) = threads.filter(|n| *n < 0) {
        eprintln!("fatal: invalid number of threads specified ({n})");
        return Ok(128);
    }
    if args.untracked && args.cached {
        eprintln!("fatal: options '--untracked' and '--cached' cannot be used together");
        return Ok(128);
    }
    // `rev... -- paths`: everything before `--` that names a revision is one; the rest are paths.
    let mut revs: Vec<String> = Vec::new();
    let mut path_args: Vec<PathBuf> = Vec::new();
    for arg in positional {
        if args.untracked
            && !arg.starts_with(':')
            && !arg.contains(['*', '?', '['])
            && std::fs::symlink_metadata(&arg).is_err()
        {
            eprintln!("fatal: {arg}: no such path in the working tree.");
            eprintln!(
                "Use 'git <command> -- <path>...' to specify paths that do not exist locally."
            );
            return Ok(128);
        }
        if path_args.is_empty()
            && !args.untracked
            && !std::path::Path::new(&arg).exists()
            && super::resolve_rev(&repository, &arg).is_ok()
        {
            revs.push(arg);
        } else {
            path_args.push(PathBuf::from(arg));
        }
    }
    path_args.extend(after_dashes.iter().map(PathBuf::from));

    let mut regexes: Vec<Regex> = Vec::new();
    for pattern in &patterns {
        let regex = if args.fixed {
            Ok(Regex::literal(pattern.as_bytes(), args.ignore_case))
        } else {
            Regex::new(pattern.as_bytes(), args.extended, args.ignore_case)
        };
        match regex {
            Ok(regex) => regexes.push(regex),
            Err(error) => {
                eprintln!("fatal: command line, '{pattern}': {error}");
                return Ok(128);
            }
        }
    }

    let cwd_prefix = {
        let root = repository.worktree()?.root().map(|p| p.to_path_buf());
        match (root, std::env::current_dir()) {
            (Some(root), Ok(here)) => {
                let relative = here.strip_prefix(&root).unwrap_or(std::path::Path::new(""));
                let bytes = super::path_arg(relative)?;
                let mut prefix = BString::from(bytes);
                if !prefix.is_empty() {
                    prefix.push(b'/');
                }
                prefix
            }
            _ => BString::from(""),
        }
    };
    let mut specs: Vec<BString> = Vec::new();
    for path in &path_args {
        specs.push(super::worktree_path(&repository, path, "grep")?);
    }
    let selected = |path: &[u8]| -> bool {
        if specs.is_empty() {
            return path.starts_with(&cwd_prefix[..]);
        }
        specs
            .iter()
            .any(|spec| rivet_core::ops::pathspec_matches(BStr::new(path), BStr::new(&spec[..])))
    };

    // The files to search, in path order.
    let mut sources: Vec<Source> = Vec::new();
    if !revs.is_empty() {
        for rev in &revs {
            let oid = super::resolve_rev(&repository, rev)?;
            let tree = repository.commit_tree(oid)?;
            for (path, (mode, blob)) in rivet_core::ops::flatten_tree(repository.odb(), tree)? {
                if mode == rivet_core::object::EntryMode::Commit || !selected(&path) {
                    continue;
                }
                let mut shown = rev.as_bytes().to_vec();
                shown.push(b':');
                shown.extend_from_slice(&path);
                sources.push(Source {
                    shown,
                    data: read_blob(&repository, &blob)?,
                });
            }
        }
    } else {
        let mut seen = std::collections::BTreeSet::new();
        for entry in repository.index().entries() {
            if entry.mode == rivet_core::object::EntryMode::Commit
                || !seen.insert(entry.path.clone())
                || !selected(&entry.path)
            {
                continue;
            }
            let shown = relative_to(&cwd_prefix, &entry.path);
            let data = if args.cached {
                read_blob(&repository, &entry.oid)?
            } else {
                match repository.worktree()?.read_file(BStr::new(&entry.path[..])) {
                    Ok(data) => data,
                    Err(_) => continue,
                }
            };
            sources.push(Source { shown, data });
        }
        if args.untracked {
            let mut options = rivet_core::worktree::StatusOptions::new();
            options.untracked(rivet_core::worktree::UntrackedMode::All);
            for path in &repository.status(&options)?.untracked {
                if !selected(path) {
                    continue;
                }
                if let Ok(data) = repository.worktree()?.read_file(BStr::new(&path[..])) {
                    sources.push(Source {
                        shown: relative_to(&cwd_prefix, path),
                        data,
                    });
                }
            }
            // Git walks the directory, so tracked and untracked files interleave by path.
            sources.sort_by(|a, b| a.shown.cmp(&b.shown));
        }
    }

    let before = args.before.or(args.context).unwrap_or(0);
    let after = args.after.or(args.context).unwrap_or(0);
    let show_name = !args.no_filename;
    let mut out: Vec<u8> = Vec::new();
    let mut any = false;
    let mut hunk_mark = false;
    let only_matching = args.only_matching && !invert;
    let mut printed_any = false;
    let name_end = if args.null { 0 } else { b'\n' };
    for source in &sources {
        let binary = !args.text && source.data[..source.data.len().min(8000)].contains(&0);
        if binary && args.skip_binary {
            continue;
        }
        let mut lines: Vec<&[u8]> = source.data.split(|b| *b == b'\n').collect();
        if source.data.ends_with(b"\n") || source.data.is_empty() {
            lines.pop();
        }
        let hits: Vec<bool> = lines
            .iter()
            .map(|line| regexes.iter().any(|r| r.is_match(line, args.word)) != invert)
            .collect();
        let count = hits.iter().filter(|h| **h).count();
        if count > 0 {
            any = true;
        }
        if quiet {
            if any {
                return Ok(0);
            }
            continue;
        }
        if args.files_without_match {
            if count == 0 {
                out.extend_from_slice(&source.shown);
                out.push(name_end);
            }
            continue;
        }
        if count == 0 {
            continue;
        }
        if args.files_with_matches {
            out.extend_from_slice(&source.shown);
            out.push(name_end);
            continue;
        }
        if args.count {
            if show_name {
                out.extend_from_slice(&source.shown);
                out.push(if args.null { 0 } else { b':' });
            }
            out.extend_from_slice(format!("{count}\n").as_bytes());
            continue;
        }
        if binary {
            if args.file_break {
                // git drops or keeps the notice depending on its worker threads and which
                // revision is searched; there is no single output to match.
                anyhow::bail!(
                    "grep: --break with a binary file that matches is not implemented (use -a, -I, -c or -l)"
                );
            }
            out.extend_from_slice(b"Binary file ");
            out.extend_from_slice(&source.shown);
            out.extend_from_slice(b" matches\n");
            continue;
        }
        // Lines to print: hits plus their context.
        let mut keep = vec![false; lines.len()];
        for (i, hit) in hits.iter().enumerate() {
            if *hit {
                let from = i.saturating_sub(before);
                let to = (i + after).min(lines.len() - 1);
                for flag in &mut keep[from..=to] {
                    *flag = true;
                }
            }
        }
        let mut last_shown = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if !keep[i] {
                continue;
            }
            let lno = i + 1;
            // The hunk mark and the `--` between non-adjacent lines come from every kept line,
            // context lines that `-o` leaves unprinted included.
            if last_shown == 0 && args.file_break {
                if printed_any {
                    out.push(b'\n');
                }
            } else if before > 0 || after > 0 {
                if last_shown == 0 {
                    if hunk_mark {
                        out.extend_from_slice(b"--\n");
                    } else {
                        hunk_mark = true;
                    }
                } else if lno > last_shown + 1 {
                    out.extend_from_slice(b"--\n");
                }
            }
            if only_matching && !hits[i] {
                continue;
            }
            if args.heading && last_shown == 0 {
                out.extend_from_slice(&source.shown);
                out.push(b'\n');
            }
            last_shown = lno;
            let sep = if args.null {
                0
            } else if hits[i] {
                b':'
            } else {
                b'-'
            };
            let prefix = |out: &mut Vec<u8>| {
                if show_name && !args.heading {
                    out.extend_from_slice(&source.shown);
                    out.push(sep);
                }
                if args.line_number {
                    out.extend_from_slice(format!("{lno}").as_bytes());
                    out.push(sep);
                }
            };
            printed_any = true;
            if only_matching {
                for (start, end) in only_matches(&regexes, line, args.word) {
                    prefix(&mut out);
                    out.extend_from_slice(&line[start..end]);
                    out.push(b'\n');
                }
            } else {
                prefix(&mut out);
                out.extend_from_slice(line);
                out.push(b'\n');
            }
        }
    }
    if !quiet {
        let mut stdout = std::io::stdout().lock();
        match stdout.write_all(&out).and_then(|_| stdout.flush()) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            other => other?,
        }
    }
    let success = if args.files_without_match {
        !out.is_empty()
    } else {
        any
    };
    Ok(if success { 0 } else { 1 })
}

/// The non-empty matches of `-o`: at each step the leftmost match of any pattern, the longest
/// one on a tie. An empty match ends the scan of the line, as in git.
fn only_matches(regexes: &[Regex], line: &[u8], word: bool) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut pos = 0;
    while pos <= line.len() {
        let mut best: Option<(usize, usize)> = None;
        for regex in regexes {
            if let Some((start, end)) = regex.find_from(line, pos, word)
                && best.is_none_or(|(s, e)| start < s || (start == s && end > e))
            {
                best = Some((start, end));
            }
        }
        match best {
            Some((start, end)) if end > start => {
                found.push((start, end));
                pos = end;
            }
            _ => break,
        }
    }
    found
}

fn read_blob(repository: &Repository, oid: &ObjectId) -> Result<Vec<u8>> {
    Ok(repository
        .odb()
        .read_typed(oid, ObjectKind::Blob)?
        .into_data())
}

/// `path` as seen from the directory `cwd` (both work-tree relative; `cwd` ends in `/` or is empty).
fn relative_to(cwd: &[u8], path: &[u8]) -> Vec<u8> {
    let base: Vec<&[u8]> = cwd
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let target: Vec<&[u8]> = path
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let shared = base
        .iter()
        .zip(&target)
        .take_while(|(a, b)| a == b)
        .count()
        .min(target.len().saturating_sub(1));
    let mut out = Vec::new();
    for _ in shared..base.len() {
        out.extend_from_slice(b"../");
    }
    out.extend_from_slice(&target[shared..].join(&b'/'));
    out
}
