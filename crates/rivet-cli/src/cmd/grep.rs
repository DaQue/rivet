//! `grep`: search tracked files in the work tree, the index (`--cached`) or a tree.
//!
//! Patterns are POSIX basic regular expressions by default, `-E` extended ones, `-F` fixed
//! strings. `-P` (Perl) is not implemented and is refused at the command line. `--and`, `--or`,
//! `--not`, `(`, `)` and `--all-match` build a boolean expression over the patterns, parsed from the
//! raw command line (the order of the words matters) with git's grammar and error texts.
//! `--threads` is validated and ignored.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use bstr::{BStr, BString};
use rivet_core::ignore::{IgnoreStack, PathKind};
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

    /// Both patterns must match the line (read from the raw command line, see `scan_expression`).
    #[arg(long = "and", action = clap::ArgAction::Count)]
    pub and_: u8,

    /// A line matching either side is a match (the default between patterns; ignored).
    #[arg(long = "or", action = clap::ArgAction::Count)]
    pub or_: u8,

    /// Negate the next pattern or group.
    #[arg(long = "not", action = clap::ArgAction::Count)]
    pub not_: u8,

    /// With several top-level patterns, a file must match every one of them.
    #[arg(long = "all-match")]
    pub all_match: bool,

    /// Search the index instead of the work tree.
    #[arg(long = "cached")]
    pub cached: bool,

    /// Also search the untracked files that are not ignored.
    #[arg(long = "untracked")]
    pub untracked: bool,

    /// Search the files of the current directory, tracked or not, in or out of a repository.
    #[arg(long = "no-index")]
    pub no_index: bool,

    /// Honour the standard ignore files (the default for `--untracked`, off for `--no-index`).
    #[arg(long = "exclude-standard")]
    pub exclude_standard: bool,

    /// Also search ignored files (only with `--untracked` or `--no-index`).
    #[arg(long = "no-exclude-standard")]
    pub no_exclude_standard: bool,

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

/// One word of the pattern expression, in command-line order.
enum Tok {
    Pattern(String),
    And,
    Not,
    Open,
    Close,
}

/// A compiled pattern expression; `Atom` indexes the list of regexes.
enum Expr {
    Atom(usize),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

impl Expr {
    fn eval(&self, regexes: &[Regex], line: &[u8], word: bool) -> bool {
        match self {
            Expr::Atom(i) => regexes[*i].is_match(line, word),
            Expr::Not(x) => !x.eval(regexes, line, word),
            Expr::And(x, y) => x.eval(regexes, line, word) && y.eval(regexes, line, word),
            Expr::Or(x, y) => x.eval(regexes, line, word) || y.eval(regexes, line, word),
        }
    }

    /// The alternatives of the outermost `Or` chain (what `--all-match` requires of a file).
    fn alternatives<'a>(&'a self, out: &mut Vec<&'a Expr>) {
        match self {
            Expr::Or(x, y) => {
                x.alternatives(out);
                y.alternatives(out);
            }
            other => out.push(other),
        }
    }
}

/// Recursive descent over `toks` with the grammar and fatal texts of git's `grep.c`.
struct ExprParser<'a> {
    toks: &'a [Tok],
    pos: usize,
    atoms: usize,
}

impl ExprParser<'_> {
    fn text(tok: &Tok) -> &str {
        match tok {
            Tok::Pattern(p) => p,
            Tok::And => "--and",
            Tok::Not => "--not",
            Tok::Open => "(",
            Tok::Close => ")",
        }
    }

    fn atom(&mut self) -> Result<Option<Expr>, String> {
        match self.toks.get(self.pos) {
            Some(Tok::Pattern(_)) => {
                self.pos += 1;
                self.atoms += 1;
                Ok(Some(Expr::Atom(self.atoms - 1)))
            }
            Some(Tok::Open) => {
                self.pos += 1;
                let inner = self.or()?;
                if !matches!(self.toks.get(self.pos), Some(Tok::Close)) {
                    return Err("unmatched ( for expression group".into());
                }
                self.pos += 1;
                Ok(inner)
            }
            _ => Ok(None),
        }
    }

    fn not(&mut self) -> Result<Option<Expr>, String> {
        if matches!(self.toks.get(self.pos), Some(Tok::Not)) {
            if self.pos + 1 >= self.toks.len() {
                return Err("--not not followed by pattern expression".into());
            }
            self.pos += 1;
            return match self.not()? {
                Some(x) => Ok(Some(Expr::Not(Box::new(x)))),
                None => Err("--not followed by non pattern expression".into()),
            };
        }
        self.atom()
    }

    fn and(&mut self) -> Result<Option<Expr>, String> {
        let x = self.not()?;
        if matches!(self.toks.get(self.pos), Some(Tok::And)) {
            let Some(x) = x else {
                return Err("--and not preceded by pattern expression".into());
            };
            if self.pos + 1 >= self.toks.len() {
                return Err("--and not followed by pattern expression".into());
            }
            self.pos += 1;
            return match self.and()? {
                Some(y) => Ok(Some(Expr::And(Box::new(x), Box::new(y)))),
                None => Err("--and not followed by pattern expression".into()),
            };
        }
        Ok(x)
    }

    fn or(&mut self) -> Result<Option<Expr>, String> {
        let Some(x) = self.and()? else {
            return Ok(None);
        };
        let Some(next) = self.toks.get(self.pos) else {
            return Ok(Some(x));
        };
        if matches!(next, Tok::Close) {
            return Ok(Some(x));
        }
        match self.or()? {
            Some(y) => Ok(Some(Expr::Or(Box::new(x), Box::new(y)))),
            None => Err(format!("not a pattern expression {}", Self::text(next))),
        }
    }
}

/// The expression words of `grep`, read from the raw command line the way git's option parser sees
/// them: they are only options before the first non-option word (after it git says `option '<w>'
/// must come before non-option arguments`). `--or` is a no-op, as in git. Returns `None` when the
/// line holds no expression word.
fn scan_expression(raw: &[String]) -> Result<Option<Vec<Tok>>, String> {
    let Some(start) = raw.iter().position(|a| a == "grep") else {
        return Ok(None);
    };
    let mut toks: Vec<Tok> = Vec::new();
    let mut special = false;
    let mut positional = false;
    let mut i = start + 1;
    while i < raw.len() {
        let word = raw[i].as_str();
        i += 1;
        if word == "--" {
            break;
        }
        let known = matches!(word, "--and" | "--or" | "--not" | "--all-match");
        if positional {
            if known {
                return Err(format!(
                    "option '{word}' must come before non-option arguments"
                ));
            }
            continue;
        }
        match word {
            "--and" => {
                toks.push(Tok::And);
                special = true;
            }
            "--not" => {
                toks.push(Tok::Not);
                special = true;
            }
            "--all-match" => special = true,
            "(" => {
                toks.push(Tok::Open);
                special = true;
            }
            ")" => {
                toks.push(Tok::Close);
                special = true;
            }
            "--or" => {}
            "--after-context" | "--before-context" | "--context" => i += 1,
            "--threads" => {
                if raw.get(i).is_some_and(|n| parse_git_int(n).is_some()) {
                    i += 1;
                }
            }
            _ if word.starts_with("--") => {}
            _ if word.starts_with('-') && word.len() > 1 => {
                // A cluster of short options: `e`, `A`, `B`, `C` take the rest or the next word.
                for (at, c) in word.char_indices().skip(1) {
                    if matches!(c, 'e' | 'A' | 'B' | 'C') {
                        let rest = &word[at + 1..];
                        let value = if rest.is_empty() {
                            i += 1;
                            raw.get(i - 1).cloned().unwrap_or_default()
                        } else {
                            rest.to_string()
                        };
                        if c == 'e' {
                            toks.push(Tok::Pattern(value));
                        }
                        break;
                    }
                }
            }
            _ => {
                positional = true;
                if toks.is_empty() {
                    toks.push(Tok::Pattern(word.to_string()));
                }
            }
        }
    }
    Ok(special.then_some(toks))
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
    let mut repository = if args.no_index {
        None
    } else {
        Some(super::open(global)?)
    };
    // `-v` and `-q` are the grammar's global flags; here they mean invert and quiet.
    let invert = args.invert || global.verbose;
    let quiet = global.quiet;
    let mut positional: Vec<String> = args.args.clone();
    let mut after_dashes: Vec<String> = args.paths.clone();
    if positional.is_empty() && args.patterns.is_empty() && !after_dashes.is_empty() {
        // `grep -- <pattern> <paths>`: the dashes only end the options.
        positional.push(after_dashes.remove(0));
    }
    let scanned = match scan_expression(&std::env::args().collect::<Vec<_>>()) {
        Ok(toks) => toks.filter(|t| !t.is_empty()),
        Err(message) => {
            eprintln!("fatal: {message}");
            return Ok(128);
        }
    };
    if scanned.is_some() {
        positional.retain(|a| a != "(" && a != ")");
    }
    let mut patterns: Vec<String> = args.patterns.clone();
    let positional_pattern = scanned
        .as_ref()
        .is_none_or(|toks| toks.iter().any(|t| matches!(t, Tok::Pattern(_))));
    if patterns.is_empty() && positional_pattern {
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
    for (set, name) in [(args.cached, "--cached"), (args.untracked, "--untracked")] {
        if args.no_index && set {
            eprintln!("fatal: options '--no-index' and '{name}' cannot be used together");
            return Ok(128);
        }
    }
    // The last of `--exclude-standard` / `--no-exclude-standard` on the line wins.
    let exclude_flag = std::env::args()
        .skip_while(|a| a != "grep")
        .take_while(|a| a != "--")
        .filter_map(|a| match a.as_str() {
            "--exclude-standard" => Some(true),
            "--no-exclude-standard" => Some(false),
            _ => None,
        })
        .last();
    // `rev... -- paths`: everything before `--` that names a revision is one; the rest are paths.
    let mut revs: Vec<String> = Vec::new();
    let mut path_args: Vec<PathBuf> = Vec::new();
    for arg in positional {
        if (args.untracked || args.no_index)
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
            && let Some(repository) = &repository
            && !std::path::Path::new(&arg).exists()
            && super::resolve_rev(repository, &arg).is_ok()
        {
            revs.push(arg);
        } else {
            path_args.push(PathBuf::from(arg));
        }
    }
    path_args.extend(after_dashes.iter().map(PathBuf::from));
    if exclude_flag.is_some() && !args.untracked && !args.no_index {
        eprintln!("fatal: --[no-]exclude-standard cannot be used for tracked contents");
        return Ok(128);
    }

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

    // The expression over the regexes, when the line has `--and`/`--not`/parentheses/`--all-match`.
    let mut expression: Option<Option<Expr>> = None;
    if let Some(toks) = &scanned {
        let mut parser = ExprParser {
            toks,
            pos: 0,
            atoms: 0,
        };
        let built = match parser.or() {
            Ok(built) => built,
            Err(message) => {
                eprintln!("fatal: {message}");
                return Ok(128);
            }
        };
        if let Some(rest) = toks.get(parser.pos) {
            eprintln!(
                "fatal: incomplete pattern expression group: {}",
                ExprParser::text(rest)
            );
            return Ok(128);
        }
        expression = Some(built);
    }
    let alternatives: Vec<&Expr> = {
        let mut out = Vec::new();
        if let Some(Some(root)) = &expression {
            root.alternatives(&mut out);
        }
        out
    };

    let cwd_prefix = {
        let root = match &repository {
            Some(repository) => repository.worktree()?.root().map(|p| p.to_path_buf()),
            None => None,
        };
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
        match &repository {
            Some(repository) => specs.push(super::worktree_path(repository, path, "grep")?),
            None => match no_index_spec(path) {
                Some(spec) => specs.push(spec),
                None => {
                    eprintln!("fatal: '{}' is outside the directory tree", path.display());
                    return Ok(128);
                }
            },
        }
    }
    // A bare `.` selects the whole directory.
    if args.no_index && specs.iter().any(|spec| spec.is_empty()) {
        specs.clear();
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
    if args.no_index {
        let mut found = Vec::new();
        let stack = if exclude_flag == Some(true) {
            standard_excludes()
        } else {
            None
        };
        let base = stack
            .as_ref()
            .map(|(_, base)| base.clone())
            .unwrap_or_default();
        walk_directory(
            std::path::Path::new("."),
            &mut Vec::new(),
            &mut found,
            stack.as_ref().map(|(stack, _)| (stack, &base[..])),
        );
        for (shown, file) in found {
            if !selected(&shown) {
                continue;
            }
            if let Ok(data) = std::fs::read(&file) {
                sources.push(Source { shown, data });
            }
        }
        sources.sort_by(|a, b| a.shown.cmp(&b.shown));
    } else if let Some(repository) = repository.as_ref().filter(|_| !revs.is_empty()) {
        for rev in &revs {
            let oid = super::resolve_rev(repository, rev)?;
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
                    data: read_blob(repository, &blob)?,
                });
            }
        }
    } else if let Some(repository) = &mut repository {
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
                read_blob(repository, &entry.oid)?
            } else {
                match repository.worktree()?.read_file(BStr::new(&entry.path[..])) {
                    Ok(data) => data,
                    Err(_) => continue,
                }
            };
            sources.push(Source { shown, data });
        }
        if args.untracked && exclude_flag == Some(false) {
            // Ignored files are searched too: every file of the work tree the index lacks.
            let tracked: std::collections::BTreeSet<Vec<u8>> = repository
                .index()
                .entries()
                .iter()
                .map(|entry| entry.path.to_vec())
                .collect();
            let root = repository.worktree()?.root().map(|p| p.to_path_buf());
            let mut found = Vec::new();
            if let Some(root) = root {
                walk_directory(&root, &mut Vec::new(), &mut found, None);
            }
            for (path, file) in found {
                if tracked.contains(&path) || !selected(&path) {
                    continue;
                }
                if let Ok(data) = std::fs::read(&file) {
                    sources.push(Source {
                        shown: relative_to(&cwd_prefix, &path),
                        data,
                    });
                }
            }
            sources.sort_by(|a, b| a.shown.cmp(&b.shown));
        } else if args.untracked {
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
        let matches_line = |line: &[u8]| match &expression {
            Some(Some(root)) => root.eval(&regexes, line, args.word),
            Some(None) => false,
            None => regexes.iter().any(|r| r.is_match(line, args.word)),
        };
        // `--all-match`: a file only counts when every top-level alternative hits some line.
        let file_ok = !args.all_match
            || alternatives
                .iter()
                .all(|alt| lines.iter().any(|l| alt.eval(&regexes, l, args.word)));
        let hits: Vec<bool> = lines
            .iter()
            .map(|line| file_ok && matches_line(line) != invert)
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

/// The path `arg` as a pattern relative to the current directory for `--no-index`: no leading `./`,
/// no trailing `/`; `None` when it climbs out of the current directory.
fn no_index_spec(arg: &std::path::Path) -> Option<BString> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for component in arg.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::Normal(name) => {
                parts.push(super::path_arg(std::path::Path::new(name)).ok()?.to_vec());
            }
            _ => return None,
        }
    }
    Some(BString::from(parts.join(&b'/')))
}

/// The standard ignore rules for a `--no-index` walk from the current directory, and the current
/// directory relative to the rules' root: the enclosing work tree when there is one, else itself.
fn standard_excludes() -> Option<(IgnoreStack, Vec<u8>)> {
    let here = std::env::current_dir().ok()?;
    let root = here
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(&here)
        .to_path_buf();
    let relative = super::path_arg(here.strip_prefix(&root).ok()?)
        .ok()?
        .to_vec();
    let mut stack = IgnoreStack::new(&root);
    if root.join(".git").is_dir() {
        stack.push_info_exclude(&root.join(".git")).ok()?;
    }
    let mut dir = Vec::new();
    stack.push_dir(BStr::new(&dir[..])).ok()?;
    for part in relative.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
        if !dir.is_empty() {
            dir.push(b'/');
        }
        dir.extend_from_slice(part);
        stack.push_dir(BStr::new(&dir[..])).ok()?;
    }
    Some((stack, relative))
}

/// Every regular file below `dir` (a `.git` entry is skipped), as (path below the start, real path).
/// With `ignore` (the rules and the start's path below their root), ignored paths are left out.
fn walk_directory(
    dir: &std::path::Path,
    prefix: &mut Vec<u8>,
    out: &mut Vec<(Vec<u8>, PathBuf)>,
    ignore: Option<(&IgnoreStack, &[u8])>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let Ok(bytes) = super::path_arg(std::path::Path::new(&name)) else {
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let mark = prefix.len();
        prefix.extend_from_slice(&bytes);
        // The path below the rules' root, for the ignore decision.
        let below_root = ignore.as_ref().map(|(_, base)| {
            let mut path = base.to_vec();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(prefix.strip_suffix(b"/").unwrap_or(prefix));
            path
        });
        if kind.is_dir() {
            let child = match (&ignore, &below_root) {
                (Some((stack, base)), Some(path)) => {
                    if !stack.should_descend(BStr::new(&path[..])).unwrap_or(true) {
                        prefix.truncate(mark);
                        continue;
                    }
                    let mut child = (*stack).clone();
                    let _ = child.push_dir(BStr::new(&path[..]));
                    Some((child, *base))
                }
                _ => None,
            };
            prefix.push(b'/');
            walk_directory(
                &entry.path(),
                prefix,
                out,
                child.as_ref().map(|(stack, base)| (stack, *base)),
            );
        } else if kind.is_file() {
            let ignored = match (&ignore, &below_root) {
                (Some((stack, _)), Some(path)) => stack
                    .is_ignored(BStr::new(&path[..]), PathKind::File)
                    .unwrap_or(false),
                _ => false,
            };
            if !ignored {
                out.push((prefix.clone(), entry.path()));
            }
        }
        prefix.truncate(mark);
    }
}
