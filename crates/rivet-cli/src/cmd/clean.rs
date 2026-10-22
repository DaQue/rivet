//! `clean`: delete untracked files from the work tree.
//!
//! The candidates come from the same status machinery `status` uses (untracked and ignored
//! paths, so `.gitignore` is honoured), and the walk over the work tree decides what is printed
//! and removed: files in tracked directories one by one, untracked directories (`-d`) whole when
//! everything inside them is a candidate, otherwise file by file. `-e` adds exclude patterns, and
//! `-i` runs git's menu (clean, filter by pattern, select by numbers, ask each, quit, help)
//! over the same candidates.

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::{Repository, UntrackedMode};

use crate::cli::GlobalArgs;

/// Arguments to `rivet clean`.
#[derive(Debug, clap::Args)]
pub struct CleanArgs {
    /// Limit cleaning to these paths (default: the current directory).
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<PathBuf>,

    /// Show what would be removed and remove nothing.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Really remove files; given twice, also remove nested repositories.
    #[arg(short = 'f', long = "force", action = clap::ArgAction::Count)]
    pub force: u8,

    /// Remove untracked directories as well as files.
    #[arg(short = 'd')]
    pub directories: bool,

    /// Also remove files that are ignored.
    #[arg(short = 'x')]
    pub ignored_too: bool,

    /// Remove only files that are ignored.
    #[arg(short = 'X')]
    pub ignored_only: bool,

    /// Interactive mode.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Also use this exclude pattern: matching paths are kept (removed with `-X`).
    #[arg(short = 'e', long = "exclude", value_name = "PATTERN")]
    pub exclude: Vec<String>,
}

/// What the walk may remove.
struct Plan<'a> {
    repository: &'a Repository,
    root: PathBuf,
    untracked: BTreeSet<BString>,
    ignored_files: BTreeSet<BString>,
    ignored_dirs: Vec<BString>,
    tracked_files: BTreeSet<BString>,
    tracked_dirs: BTreeSet<BString>,
    specs: Vec<BString>,
    args: &'a CleanArgs,
    excludes: Vec<rivet_core::ignore::Pattern>,
    found: Vec<(BString, bool)>,
}

impl Plan<'_> {
    fn is_ignored(&self, path: &BStr) -> bool {
        self.ignored_files.contains(path)
            || self.ignored_dirs.iter().any(|d| {
                let d: &[u8] = d;
                path.starts_with(d)
            })
    }

    /// Whether a `-e` pattern matches the file or one of its directories.
    fn excluded(&self, path: &BStr) -> bool {
        use rivet_core::ignore::PathKind;
        if self.excludes.is_empty() {
            return false;
        }
        let bytes: &[u8] = path;
        let mut end = 0;
        while let Some(slash) = bytes[end..].find_byte(b'/') {
            end += slash;
            let dir = BStr::new(&bytes[..end]);
            if self
                .excludes
                .iter()
                .any(|p| p.matches(dir, PathKind::Directory))
            {
                return true;
            }
            end += 1;
        }
        self.excludes
            .iter()
            .any(|p| p.matches(path, PathKind::File))
    }

    /// Whether the user's rules make this file a deletion candidate.
    fn candidate(&self, path: &BStr) -> bool {
        let ignored = self.is_ignored(path);
        let excluded = self.excluded(path);
        if self.args.ignored_only {
            ignored || excluded
        } else if self.args.ignored_too {
            (ignored || self.untracked.contains(path)) && !excluded
        } else {
            !ignored && self.untracked.contains(path) && !excluded
        }
    }

    fn in_spec(&self, path: &BStr) -> bool {
        self.specs
            .iter()
            .any(|spec| rivet_core::ops::pathspec_matches(path, BStr::new(&spec[..])))
    }

    /// Whether `dir` (no trailing slash) holds only candidates, recursively, and is wholly
    /// inside the requested paths.
    fn collapsible(&self, dir: &Path, rel: &BStr) -> Result<bool> {
        if !self.in_spec(rel) {
            return Ok(false);
        }
        // With -X only ignored directories are removed whole; others are emptied file by file.
        if self.args.ignored_only {
            let mut as_dir = rel.to_vec();
            as_dir.push(b'/');
            if !self.ignored_dirs.iter().any(|d| as_dir.starts_with(d)) {
                return Ok(false);
            }
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let child_rel = join(rel, name.as_bytes());
            let child_rel = BStr::new(&child_rel[..]);
            if entry.file_type()?.is_dir() {
                if name.as_bytes() == b".git" {
                    return Ok(false);
                }
                if self.tracked_dirs.contains(child_rel) {
                    return Ok(false);
                }
                if entry.path().join(".git").exists() {
                    return Ok(false);
                }
                if !self.collapsible(&entry.path(), child_rel)? {
                    return Ok(false);
                }
            } else if !self.candidate(child_rel) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn walk(&mut self, dir: &Path, rel: &BStr, in_untracked_dir: bool) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name().as_bytes().to_vec());
        for entry in entries {
            let name = entry.file_name();
            if name.as_bytes() == b".git" {
                continue;
            }
            let child_rel = join(rel, name.as_bytes());
            let child = BStr::new(&child_rel[..]);
            if entry.file_type()?.is_dir() {
                if self.tracked_dirs.contains(child) {
                    self.walk(&entry.path(), child, false)?;
                    continue;
                }
                // An untracked directory: whole with -d; with -X its ignored files are still found.
                if !self.args.directories {
                    let mut as_dir = child_rel.to_vec();
                    as_dir.push(b'/');
                    let dir_ignored = self.ignored_dirs.iter().any(|d| as_dir.starts_with(d));
                    if self.args.ignored_only && !dir_ignored && !entry.path().join(".git").exists()
                    {
                        self.walk(&entry.path(), child, true)?;
                    }
                    continue;
                }
                if entry.path().join(".git").exists() {
                    if self.args.force >= 2 && !self.args.ignored_only && self.in_spec(child) {
                        self.found.push((child_rel.clone(), true));
                    }
                    continue;
                }
                if self.collapsible(&entry.path(), child)? {
                    self.found.push((child_rel.clone(), true));
                } else {
                    self.walk(&entry.path(), child, true)?;
                }
            } else if (!in_untracked_dir || self.args.directories || self.args.ignored_only)
                && !self.tracked_files.contains(child)
                && self.candidate(child)
                && self.in_spec(child)
            {
                self.found.push((child_rel.clone(), false));
            }
        }
        Ok(())
    }
}

fn join(dir: &BStr, name: &[u8]) -> BString {
    let mut out = BString::from(dir.as_bytes());
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend_from_slice(name);
    out
}

/// `path` as seen from the directory `cwd` (both work-tree relative, `/`-separated).
pub(super) fn relative_to(cwd: &[u8], path: &[u8]) -> Vec<u8> {
    let base: Vec<&[u8]> = cwd
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let target: Vec<&[u8]> = path
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let shared = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut out = Vec::new();
    for _ in shared..base.len() {
        out.extend_from_slice(b"../");
    }
    let rest = target[shared..].join(&b'/');
    out.extend_from_slice(&rest);
    if path.ends_with(b"/") && !out.ends_with(b"/") {
        out.push(b'/');
    }
    out
}

/// Handler for `rivet clean`.
///
/// # Exit codes
///
/// * `0` — done (or nothing to do).
/// * `1` — some path could not be removed.
/// * `128` — neither `-n` nor `-f` was given (and `clean.requireForce` is not false), or an
///   unsupported option was used.
pub fn run_clean(global: &GlobalArgs, args: &CleanArgs) -> Result<i32> {
    let mut repository = super::open(global)?;
    let require_force = repository
        .config_value(BStr::new("clean.requireForce"))?
        .map(|v| rivet_core::parse_bool(BStr::new(&v[..])).unwrap_or(true))
        .unwrap_or(true);
    if require_force && !args.dry_run && args.force == 0 && !args.interactive {
        anyhow::bail!("clean.requireForce is true and -f not given: refusing to clean");
    }
    if args.ignored_too && args.ignored_only {
        anyhow::bail!("clean: -x and -X cannot be used together");
    }
    let root = repository
        .worktree()?
        .root()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("clean: this operation must be run in a work tree"))?;

    let cwd_prefix = {
        let here = std::env::current_dir()?;
        let relative = here.strip_prefix(&root).unwrap_or(Path::new(""));
        BString::from(super::path_arg(relative)?)
    };
    let mut specs = Vec::new();
    for path in &args.paths {
        specs.push(super::worktree_path(&repository, path, "clean")?);
    }
    if specs.is_empty() {
        specs.push(if cwd_prefix.is_empty() {
            BString::from(".")
        } else {
            cwd_prefix.clone()
        });
    }

    let mut found = plan_found(&mut repository, args, specs)?;
    found.sort_by_key(display_key);
    if args.interactive {
        let names: Vec<String> = found
            .iter()
            .map(|(path, dir)| {
                let mut full = path.to_vec();
                if *dir {
                    full.push(b'/');
                }
                let label = rivet_core::ops::quote_path(&relative_to(&cwd_prefix, &full), false);
                String::from_utf8_lossy(&label).into_owned()
            })
            .collect();
        let chosen = interactive::choose(&names)?;
        let mut kept = Vec::new();
        for (index, item) in found.into_iter().enumerate() {
            if chosen.contains(&index) {
                kept.push(item);
            }
        }
        found = kept;
    }

    use std::io::Write as _;
    let mut out = Vec::new();
    let mut failed = false;
    let shown = |path: &BString, dir: bool| {
        let mut full = path.to_vec();
        if dir {
            full.push(b'/');
        }
        rivet_core::ops::quote_path(&relative_to(&cwd_prefix, &full), false)
    };
    for (path, dir) in &found {
        let label = shown(path, *dir);
        if args.dry_run {
            out.extend_from_slice(b"Would remove ");
            out.extend_from_slice(&label);
            out.push(b'\n');
            continue;
        }
        let target = root.join(std::ffi::OsStr::from_bytes(path));
        let result = if *dir {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
        match result {
            Ok(()) => {
                if !global.quiet {
                    out.extend_from_slice(b"Removing ");
                    out.extend_from_slice(&label);
                    out.push(b'\n');
                }
            }
            Err(error) => {
                eprintln!("warning: failed to remove {}: {error}", target.display());
                failed = true;
            }
        }
    }
    std::io::stdout().write_all(&out)?;
    Ok(if failed { 1 } else { 0 })
}

/// The paths `clean` would remove for `args`, limited to the work-tree relative `specs`.
fn plan_found(
    repository: &mut Repository,
    args: &CleanArgs,
    specs: Vec<BString>,
) -> Result<Vec<(BString, bool)>> {
    let root = repository
        .worktree()?
        .root()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("clean: this operation must be run in a work tree"))?;
    let mut options = rivet_core::StatusOptions::new();
    options.untracked_files = UntrackedMode::All;
    options.ignored = true;
    options.refresh = false;
    let status = repository.status(&options)?;
    let mut ignored_files = BTreeSet::new();
    let mut ignored_dirs = Vec::new();
    for path in &status.ignored {
        if path.ends_with(b"/") {
            ignored_dirs.push(path.clone());
        } else {
            ignored_files.insert(path.clone());
        }
    }
    let mut tracked_files = BTreeSet::new();
    let mut tracked_dirs = BTreeSet::new();
    for entry in repository.index().entries() {
        tracked_files.insert(entry.path.clone());
        let mut end = 0;
        while let Some(slash) = entry.path[end..].find_byte(b'/') {
            end += slash;
            tracked_dirs.insert(BString::from(&entry.path[..end]));
            end += 1;
        }
    }

    let repository: &Repository = repository;
    let mut excludes = Vec::new();
    for text in &args.exclude {
        if let Some(pattern) = rivet_core::ignore::Pattern::parse(
            BStr::new(text.as_bytes()),
            rivet_core::ignore::IgnoreSource::CommandLine,
            0,
            BStr::new(""),
        )? {
            excludes.push(pattern);
        }
    }
    let mut plan = Plan {
        repository,
        root: root.clone(),
        untracked: status.untracked.iter().cloned().collect(),
        ignored_files,
        ignored_dirs,
        tracked_files,
        tracked_dirs,
        specs,
        args,
        excludes,
        found: Vec::new(),
    };
    let _ = (&plan.repository, &plan.root);
    plan.walk(&root, BStr::new(""), false)?;
    Ok(std::mem::take(&mut plan.found))
}

fn stash_clean_args(include_ignored: bool) -> CleanArgs {
    CleanArgs {
        paths: Vec::new(),
        dry_run: false,
        force: 1,
        directories: true,
        ignored_too: include_ignored,
        ignored_only: false,
        interactive: false,
        exclude: Vec::new(),
    }
}

fn root_specs(repository: &Repository, paths: &[PathBuf]) -> Result<Vec<BString>> {
    let mut specs = Vec::new();
    for path in paths {
        specs.push(super::worktree_path(repository, path, "stash")?);
    }
    if specs.is_empty() {
        specs.push(BString::from("."));
    }
    Ok(specs)
}

/// What `git clean -d [-x]` would remove for `stash -u/-a`, limited to `paths` (default: the
/// whole work tree); directories are reported whole.
pub(super) fn untracked_candidates(
    repository: &mut Repository,
    paths: &[PathBuf],
    include_ignored: bool,
) -> Result<Vec<(BString, bool)>> {
    let specs = root_specs(repository, paths)?;
    plan_found(repository, &stash_clean_args(include_ignored), specs)
}

/// Every file below the work-tree directory `dir`, appended to `out`.
pub(super) fn files_under(
    repository: &Repository,
    dir: &BString,
    out: &mut Vec<BString>,
) -> Result<()> {
    let root = repository
        .worktree()?
        .root()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
    let mut stack = vec![dir.clone()];
    while let Some(current) = stack.pop() {
        let full = root.join(std::ffi::OsStr::from_bytes(&current));
        for entry in std::fs::read_dir(&full)? {
            let entry = entry?;
            let child = join(BStr::new(&current[..]), entry.file_name().as_bytes());
            if entry.file_type()?.is_dir() {
                stack.push(child);
            } else {
                out.push(child);
            }
        }
    }
    Ok(())
}

/// Remove what [`untracked_candidates`] reports (`stash -u/-a` after saving).
pub(super) fn remove_untracked(
    repository: &mut Repository,
    paths: &[PathBuf],
    include_ignored: bool,
) -> Result<()> {
    let root = repository
        .worktree()?
        .root()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
    for (path, dir) in untracked_candidates(repository, paths, include_ignored)? {
        let target = root.join(std::ffi::OsStr::from_bytes(&path));
        if dir {
            std::fs::remove_dir_all(&target)?;
        } else {
            std::fs::remove_file(&target)?;
        }
    }
    Ok(())
}

/// `clean -i`: git's menu over the candidate list.
mod interactive {
    use std::io::{BufRead as _, Write as _};

    const HELP: &str = "clean               - start cleaning\nfilter by pattern   - exclude items from deletion\nselect by numbers   - select items to be deleted by numbers\nask each            - confirm each deletion (like \"rm -i\")\nquit                - stop cleaning\nhelp                - this screen\n?                   - help for prompt selection\n";
    const PROMPT_HELP: &str = "Prompt help:\n1          - select a numbered item\nfoo        - select item based on unique prefix\n           - (empty) select nothing\n";
    const NAMES: [&str; 6] = [
        "clean",
        "filter by pattern",
        "select by numbers",
        "ask each",
        "quit",
        "help",
    ];

    fn prompt(text: &str) -> Option<String> {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_string()),
        }
    }

    /// `items` in rows of equal-width cells, git's `print_columns` (row layout).
    fn grid(items: &[String], indent: &str, padding: usize) -> String {
        let width = terminal_width();
        let cell = items.iter().map(|i| i.chars().count()).max().unwrap_or(0) + padding;
        let cols = (width.saturating_sub(indent.len()) / cell).max(1);
        let mut out = String::new();
        for row in items.chunks(cols) {
            out.push_str(indent);
            for (n, item) in row.iter().enumerate() {
                out.push_str(item);
                if n + 1 < row.len() {
                    out.push_str(&" ".repeat(cell - item.chars().count()));
                }
            }
            out.push('\n');
        }
        out
    }

    /// The candidate list: cells fill down the columns first.
    fn columns(items: &[String]) -> String {
        let width = terminal_width();
        let cell = items.iter().map(|i| i.chars().count()).max().unwrap_or(0) + 2;
        let cols = (width.saturating_sub(2) / cell).max(1);
        let rows = items.len().div_ceil(cols).max(1);
        let cols = items.len().div_ceil(rows).max(1);
        let mut out = String::new();
        for row in 0..rows {
            out.push_str("  ");
            for col in 0..cols {
                let Some(item) = items.get(col * rows + row) else {
                    continue;
                };
                out.push_str(item);
                if (col + 1) * rows + row < items.len() {
                    out.push_str(&" ".repeat(cell - item.chars().count()));
                }
            }
            out.push('\n');
        }
        out
    }

    fn terminal_width() -> usize {
        std::env::var("COLUMNS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|w| *w > 0)
            .unwrap_or(80)
    }

    fn menu() -> String {
        let items: Vec<String> = [
            "1: clean",
            "2: filter by pattern",
            "3: select by numbers",
            "4: ask each",
            "5: quit",
            "6: help",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        format!("*** Commands ***\n{}", grid(&items, "    ", 4))
    }

    fn numbered(items: &[String], selected: &[bool]) -> String {
        let cells: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, name)| {
                format!(
                    "{}{:2}: {}",
                    if selected[i] { '*' } else { ' ' },
                    i + 1,
                    name
                )
            })
            .collect();
        grid(&cells, "  ", 2)
    }

    /// Run the menu; returns the indexes (into `names`) to remove.
    pub(super) fn choose(names: &[String]) -> anyhow::Result<Vec<usize>> {
        let mut live: Vec<usize> = (0..names.len()).collect();
        if live.is_empty() {
            return Ok(live);
        }
        let mut show_list = true;
        loop {
            let shown: Vec<String> = live.iter().map(|&i| names[i].clone()).collect();
            if show_list {
                let noun = if live.len() == 1 { "item" } else { "items" };
                print!("Would remove the following {noun}:\n{}", columns(&shown));
            }
            show_list = true;
            print!("{}", menu());
            let Some(line) = prompt("What now> ") else {
                println!("Bye.");
                return Ok(Vec::new());
            };
            if line.is_empty() {
                show_list = false;
                continue;
            }
            let choice = match line.parse::<usize>() {
                Ok(n) if (1..=6).contains(&n) => Some(n - 1),
                Ok(_) => None,
                Err(_) => {
                    let lower = line.to_lowercase();
                    if lower == "?" {
                        print!("{PROMPT_HELP}");
                        show_list = false;
                        continue;
                    } else {
                        let found: Vec<usize> =
                            (0..6).filter(|&i| NAMES[i].starts_with(&lower)).collect();
                        if found.len() == 1 {
                            Some(found[0])
                        } else {
                            None
                        }
                    }
                }
            };
            match choice {
                None => {
                    println!("Huh ({line})?");
                    show_list = false;
                }
                Some(0) => return Ok(live),
                Some(4) => {
                    println!("Bye.");
                    return Ok(Vec::new());
                }
                Some(5) => print!("{HELP}"),
                Some(1) => {
                    while !live.is_empty() {
                        let shown: Vec<String> = live.iter().map(|&i| names[i].clone()).collect();
                        print!("{}", columns(&shown));
                        let Some(line) = prompt("Input ignore patterns>> ") else {
                            break;
                        };
                        if line.is_empty() {
                            break;
                        }
                        let patterns: Vec<rivet_core::ignore::Pattern> = line
                            .split_whitespace()
                            .filter_map(|text| {
                                rivet_core::ignore::Pattern::parse(
                                    bstr::BStr::new(text.as_bytes()),
                                    rivet_core::ignore::IgnoreSource::CommandLine,
                                    0,
                                    bstr::BStr::new(""),
                                )
                                .ok()
                                .flatten()
                            })
                            .collect();
                        live.retain(|&i| {
                            let name = names[i].as_str();
                            let (path, kind) = match name.strip_suffix('/') {
                                Some(dir) => (dir, rivet_core::ignore::PathKind::Directory),
                                None => (name, rivet_core::ignore::PathKind::File),
                            };
                            !patterns
                                .iter()
                                .any(|p| p.matches(bstr::BStr::new(path.as_bytes()), kind))
                        });
                    }
                }
                Some(2) => {
                    let shown: Vec<String> = live.iter().map(|&i| names[i].clone()).collect();
                    let mut selected = vec![false; shown.len()];
                    print!("{}", numbered(&shown, &[false].repeat(shown.len())));
                    while let Some(line) = prompt("Select items to delete>> ") {
                        if line.is_empty() {
                            break;
                        }
                        for token in line.split([',', ' ']).filter(|t| !t.is_empty()) {
                            let (clear, body) = match token.strip_prefix('-') {
                                Some(rest) => (true, rest),
                                None => (false, token),
                            };
                            let range = if body == "*" {
                                Some((1, shown.len()))
                            } else if let Some((a, b)) = body.split_once('-') {
                                a.parse::<usize>().ok().zip(b.parse::<usize>().ok())
                            } else {
                                body.parse::<usize>().ok().map(|n| (n, n))
                            };
                            match range {
                                Some((a, b)) if a >= 1 && a <= b && b <= shown.len() => {
                                    for flag in &mut selected[a - 1..b] {
                                        *flag = !clear;
                                    }
                                }
                                _ => println!("Huh ({token})?"),
                            }
                        }
                        print!("{}", numbered(&shown, &selected));
                    }
                    let mut keep = selected.into_iter();
                    live.retain(|_| keep.next().unwrap_or(false));
                }
                Some(3) => {
                    let mut yes = Vec::new();
                    let mut eof = false;
                    for &i in &live {
                        match prompt(&format!("Remove {} [y/N]? ", names[i])) {
                            None => {
                                println!();
                                eof = true;
                                break;
                            }
                            Some(answer) => {
                                let answer = answer.to_lowercase();
                                if answer == "y" || answer == "yes" {
                                    yes.push(i);
                                }
                            }
                        }
                    }
                    let _ = eof;
                    return Ok(yes);
                }
                Some(_) => unreachable!("six commands"),
            }
            if live.is_empty() {
                println!("No more files to clean, exiting.");
                return Ok(live);
            }
        }
    }
}

fn display_key(item: &(BString, bool)) -> Vec<u8> {
    let mut key = item.0.to_vec();
    if item.1 {
        key.push(b'/');
    }
    key
}
