//! `diff-tree`: compare the content and mode of blobs found via two tree objects.

use std::path::PathBuf;

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::object::{Commit, EntryMode, Tree, TreeEntry};
use rivet_core::{ObjectId, ObjectKind, Repository};

use crate::cli::GlobalArgs;

/// Arguments of `diff-tree`: everything after the command, in Git's own grammar.
#[derive(Debug, clap::Args)]
pub struct DiffTreeArgs {
    /// `[<options>] <tree-ish> [<tree-ish>] [--] [<path>...]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE: &str = include_str!("difftree_usage.txt");

#[derive(Default)]
struct Opts {
    recursive: bool,
    show_trees: bool,
    root: bool,
    no_commit_id: bool,
    merges: bool,
    silent: bool,
    name_only: bool,
    name_status: bool,
    stat: bool,
    numstat: bool,
    raw: bool,
    full_index: bool,
    stdin: bool,
    patch: bool,
    nul: bool,
    abbrev: Option<Option<usize>>,
    /// `-M`: the similarity a rename needs, on Git's scale.
    rename_score: Option<u64>,
    /// `-C` / `--find-copies-harder`.
    copies: bool,
    harder: bool,
}

/// A `-M<n>` / `-C<n>` score: `50%` is a percentage, bare digits are the digits of a fraction
/// (`5` and `50` are both one half, `05` is five percent).
fn parse_score(text: &str) -> Option<u64> {
    let max = u128::from(rivet_core::diff::MAX_SCORE);
    if let Some(percent) = text.strip_suffix('%') {
        let (whole, fraction) = percent.split_once('.').unwrap_or((percent, ""));
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        let digits = format!("{whole}{fraction}");
        let value: u128 = digits.parse().ok()?;
        let scale = 10u128.pow(fraction.len() as u32);
        return Some((value * max / (100 * scale)) as u64);
    }
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u128 = text.parse().ok()?;
    let scale = 10u128.pow(text.len() as u32);
    Some((value * max / scale) as u64)
}

/// One line of raw output.
struct Rec {
    omode: Option<EntryMode>,
    nmode: Option<EntryMode>,
    ooid: Option<ObjectId>,
    noid: Option<ObjectId>,
    status: u8,
    path: BString,
    /// The source path of a rename or copy, and its similarity percentage.
    old_path: Option<BString>,
    score: Option<u32>,
}

struct Walker<'a> {
    repository: &'a Repository,
    recursive: bool,
    show_trees: bool,
    specs: &'a [BString],
    out: Vec<Rec>,
}

/// Git's tree order: a directory sorts as if its name ended in `/`.
fn key(entry: &TreeEntry) -> Vec<u8> {
    let mut key = entry.name.to_vec();
    if entry.mode == EntryMode::Tree {
        key.push(b'/');
    }
    key
}

fn class(mode: EntryMode) -> u8 {
    match mode {
        EntryMode::Link => 1,
        EntryMode::Commit => 2,
        _ => 0,
    }
}

impl Walker<'_> {
    fn wanted(&self, path: &BStr, is_tree: bool) -> bool {
        self.specs.is_empty()
            || self.specs.iter().any(|spec| {
                let spec = spec.strip_suffix(b"/").unwrap_or(spec);
                rivet_core::ops::pathspec_matches(path, BStr::new(spec))
                    || (is_tree
                        && spec.len() > path.len()
                        && spec.starts_with(path)
                        && spec[path.len()] == b'/')
            })
    }

    fn entries(&self, oid: Option<ObjectId>) -> Result<Vec<TreeEntry>> {
        let Some(oid) = oid else {
            return Ok(Vec::new());
        };
        let data = self
            .repository
            .odb()
            .read_typed(&oid, ObjectKind::Tree)?
            .into_data();
        Ok(Tree::parse(self.repository.format(), &data)?.entries)
    }

    fn list_tree(&self, is_tree: bool) -> bool {
        is_tree && (!self.recursive || self.show_trees)
    }

    fn one_sided(&mut self, entry: &TreeEntry, is_old: bool, prefix: &BStr) -> Result<()> {
        let mut path = prefix.to_vec();
        path.extend_from_slice(&entry.name);
        let path = BString::from(path);
        let is_tree = entry.mode == EntryMode::Tree;
        let wanted = self.wanted(path.as_bstr(), is_tree);
        if (self.list_tree(is_tree) || !is_tree) && wanted {
            self.out.push(Rec {
                omode: is_old.then_some(entry.mode),
                nmode: (!is_old).then_some(entry.mode),
                ooid: is_old.then_some(entry.oid),
                noid: (!is_old).then_some(entry.oid),
                status: if is_old { b'D' } else { b'A' },
                path: path.clone(),
                old_path: None,
                score: None,
            });
        }
        if is_tree && self.recursive {
            let mut child = path.to_vec();
            child.push(b'/');
            let (old, new) = if is_old {
                (Some(entry.oid), None)
            } else {
                (None, Some(entry.oid))
            };
            self.walk(old, new, BStr::new(&child))?;
        }
        Ok(())
    }

    fn walk(&mut self, old: Option<ObjectId>, new: Option<ObjectId>, prefix: &BStr) -> Result<()> {
        let a = self.entries(old)?;
        let b = self.entries(new)?;
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            let ordering = match (a.get(i), b.get(j)) {
                (Some(x), Some(y)) => key(x).cmp(&key(y)),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            match ordering {
                std::cmp::Ordering::Less => {
                    self.one_sided(&a[i], true, prefix)?;
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    self.one_sided(&b[j], false, prefix)?;
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    let (x, y) = (&a[i], &b[j]);
                    i += 1;
                    j += 1;
                    if x.oid == y.oid && x.mode == y.mode {
                        continue;
                    }
                    let mut path = prefix.to_vec();
                    path.extend_from_slice(&x.name);
                    let path = BString::from(path);
                    let is_tree = x.mode == EntryMode::Tree;
                    if (self.list_tree(is_tree) || !is_tree) && self.wanted(path.as_bstr(), is_tree)
                    {
                        let status = if class(x.mode) != class(y.mode) {
                            b'T'
                        } else {
                            b'M'
                        };
                        self.out.push(Rec {
                            omode: Some(x.mode),
                            nmode: Some(y.mode),
                            ooid: Some(x.oid),
                            noid: Some(y.oid),
                            status,
                            path: path.clone(),
                            old_path: None,
                            score: None,
                        });
                    }
                    if is_tree && self.recursive {
                        let mut child = path.to_vec();
                        child.push(b'/');
                        self.walk(Some(x.oid), Some(y.oid), BStr::new(&child))?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn write_stdout(bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|_| stdout.flush()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Into::into),
    }
}

fn ambiguous(spec: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "ambiguous argument '{spec}': unknown revision or path not in the working tree.\nUse '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
    )
}

/// What a `<tree-ish>` argument names.
enum Named {
    Commit(ObjectId, ObjectId, Vec<ObjectId>),
    Tree(ObjectId),
    Other(ObjectId, &'static str),
}

fn name(repository: &Repository, spec: &str) -> Result<Named> {
    let resolve = |spec: &str| {
        rivet_core::revparse::resolve_path(
            repository.format(),
            repository.refs(),
            repository.odb(),
            BStr::new(spec.as_bytes()),
        )
        .map_err(|_| ambiguous(spec))
    };
    let mut oid = resolve(spec)?;
    let mut object = repository.odb().read(&oid)?;
    if object.kind() == ObjectKind::Tag {
        oid =
            super::resolve_rev(repository, &format!("{spec}^{{}}")).map_err(|_| ambiguous(spec))?;
        object = repository.odb().read(&oid)?;
    }
    Ok(match object.kind() {
        ObjectKind::Commit => {
            let commit = Commit::parse(repository.format(), object.data())?;
            let parents = repository.odb().visible_parents(&oid, &commit);
            Named::Commit(oid, commit.tree, parents)
        }
        ObjectKind::Tree => Named::Tree(oid),
        ObjectKind::Blob => Named::Other(oid, "blob"),
        _ => Named::Other(oid, "tag"),
    })
}

fn tree_of(named: &Named) -> Result<ObjectId> {
    match named {
        Named::Commit(_, tree, _) => Ok(*tree),
        Named::Tree(oid) => Ok(*oid),
        Named::Other(oid, kind) => anyhow::bail!("object {oid} is a {kind}, not a tree"),
    }
}

/// `rivet diff-tree [options] <tree-ish> [<tree-ish>] [--] [<path>...]`.
///
/// # Caveats
///
/// * **Raw, `--name-only`, `--name-status`, `--stat` and `-p` output are implemented**, with
///   `-r`, `-t`, `--root`, `--no-commit-id`, `-m`, `-s`, `-z` and `--abbrev[=<n>]`. `--stat`,
///   `--numstat` and `-p` are always recursive, as in Git; `--raw`, `--numstat`, `--stat` and `-p`
///   (also as `--patch-with-raw` / `--patch-with-stat`) combine in that order with a blank line
///   before the patch, and `--name-only` wins over `--name-status` wins over the patch formats.
///   `--full-index` widens the `index` line of a patch. `--stdin` reads one commit (or
///   `<commit> <parent>...`, or `<tree> <tree>`) per line after the command-line ones and echoes
///   a line that names nothing.
/// * **`--first-parent` changes nothing**: Git 2.x prints no merge diffs for it without `-m`.
/// * **Not implemented, refused by name:** `-c`/`--cc` and rename and copy detection.
pub fn run_diff_tree(global: &GlobalArgs, args: &DiffTreeArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let format = repository.format();
    let raw: Vec<String> = std::env::args()
        .skip_while(|word| word != "diff-tree")
        .skip(1)
        .collect();
    let words = if raw.is_empty() { &args.args } else { &raw };

    let mut opts = Opts::default();
    let mut positional: Vec<&str> = Vec::new();
    let mut paths: Vec<&str> = Vec::new();
    let mut dashdash = false;
    for word in words {
        if dashdash {
            paths.push(word);
            continue;
        }
        match word.as_str() {
            "--" => dashdash = true,
            "-r" => opts.recursive = true,
            "-t" => {
                opts.show_trees = true;
                opts.recursive = true;
            }
            "--root" => opts.root = true,
            "--no-commit-id" => opts.no_commit_id = true,
            "-m" => opts.merges = true,
            "--first-parent" => {}
            "-s" | "--no-patch" => opts.silent = true,
            "--name-only" => opts.name_only = true,
            "--name-status" => opts.name_status = true,
            "--stat" => opts.stat = true,
            "--numstat" => opts.numstat = true,
            "--patch-with-raw" => {
                opts.patch = true;
                opts.raw = true;
            }
            "--patch-with-stat" => {
                opts.patch = true;
                opts.stat = true;
            }
            "--full-index" => opts.full_index = true,
            "--stdin" => opts.stdin = true,
            "-p" | "-u" | "--patch" => opts.patch = true,
            "-z" => opts.nul = true,
            "--raw" => opts.raw = true,
            "--abbrev" => opts.abbrev = Some(None),
            "-c" | "--cc" => {
                anyhow::bail!("diff-tree: {word} is not implemented");
            }
            "--no-renames" => {
                opts.rename_score = None;
                opts.copies = false;
            }
            "-M" | "--find-renames" => {
                // A later -M leaves renames only: copy detection is switched off again.
                opts.copies = false;
                opts.rename_score = Some(rivet_core::diff::rename::MINIMUM_SCORE);
            }
            "-C" | "--find-copies" => {
                opts.copies = true;
                opts.rename_score
                    .get_or_insert(rivet_core::diff::rename::MINIMUM_SCORE);
            }
            "--find-copies-harder" => {
                opts.copies = true;
                opts.harder = true;
                opts.rename_score
                    .get_or_insert(rivet_core::diff::rename::MINIMUM_SCORE);
            }
            other
                if (other.starts_with("-M") || other.starts_with("--find-renames="))
                    && !other.starts_with("--find-copies") =>
            {
                let text = other.strip_prefix("--find-renames=").unwrap_or(&other[2..]);
                let Some(score) = parse_score(text) else {
                    eprintln!("error: invalid argument to find-renames");
                    return Ok(129);
                };
                opts.copies = false;
                opts.rename_score = Some(score);
            }
            other if other.starts_with("-C") || other.starts_with("--find-copies=") => {
                let text = other.strip_prefix("--find-copies=").unwrap_or(&other[2..]);
                let Some(score) = parse_score(text) else {
                    eprintln!("error: invalid argument to find-copies");
                    return Ok(129);
                };
                opts.copies = true;
                opts.rename_score = Some(score);
            }
            other if other.starts_with("--abbrev=") => {
                let Ok(n) = other["--abbrev=".len()..].parse::<usize>() else {
                    anyhow::bail!("diff-tree: bad --abbrev argument");
                };
                opts.abbrev = Some(Some(n));
            }
            other if other.starts_with('-') && other.len() > 1 => {
                eprint!("{USAGE}");
                return Ok(129);
            }
            other => positional.push(other),
        }
    }
    // As in Git, `--find-copies-harder` turns copy detection on whatever else was said.
    if opts.harder {
        opts.copies = true;
        opts.rename_score
            .get_or_insert(rivet_core::diff::rename::MINIMUM_SCORE);
    }
    if positional.is_empty() && !opts.stdin {
        eprint!("{USAGE}");
        return Ok(129);
    }

    // Revisions first, then anything that exists on disk is a path; anything else is an error.
    let mut named: Vec<Named> = Vec::new();
    for (at, spec) in positional.iter().enumerate() {
        if dashdash || at < 2 {
            match name(&repository, spec) {
                Ok(item) => {
                    named.push(item);
                    continue;
                }
                Err(error) if at == 0 || dashdash => return Err(error),
                Err(error) => {
                    if !std::path::Path::new(spec).exists() {
                        return Err(error);
                    }
                }
            }
        } else if !std::path::Path::new(spec).exists() {
            return Err(ambiguous(spec));
        }
        paths.push(spec);
    }
    let mut specs: Vec<BString> = Vec::new();
    for path in paths {
        specs.push(
            super::worktree_path(&repository, &PathBuf::from(path), "diff-tree")
                .unwrap_or_else(|_| BString::from(path)),
        );
    }

    let commit_jobs = |oid: &ObjectId, tree: &ObjectId, parents: &[ObjectId]| -> Result<Vec<Job>> {
        let mut jobs: Vec<Job> = Vec::new();
        let header = Some(oid.to_string().into_bytes());
        if parents.is_empty() {
            if opts.root {
                jobs.push((header, None, *tree));
            }
        } else if parents.len() == 1 || opts.merges {
            for parent in parents {
                let parent_tree = match name(&repository, &parent.to_string())? {
                    Named::Commit(_, parent_tree, _) => parent_tree,
                    _ => continue,
                };
                jobs.push((header.clone(), Some(parent_tree), *tree));
            }
        }
        Ok(jobs)
    };

    // (header, old tree, new tree) per comparison.
    let mut jobs: Vec<Job> = Vec::new();
    if named.len() == 2 {
        jobs.push((None, Some(tree_of(&named[0])?), tree_of(&named[1])?));
    } else if let Some(first) = named.first() {
        match first {
            Named::Commit(oid, tree, parents) => jobs = commit_jobs(oid, tree, parents)?,
            Named::Tree(oid) => eprintln!("error: object {oid} is a tree, not a commit"),
            Named::Other(oid, kind) => eprintln!("error: object {oid} is a {kind}, not a commit"),
        }
    }

    let minimum = opts
        .abbrev
        .map(|requested| super::history::abbrev_minimum(&repository, requested));
    let show = |oid: &ObjectId| -> String {
        match minimum {
            Some(min) => repository
                .odb()
                .abbreviate(oid, min)
                .unwrap_or_else(|_| oid.to_string()),
            None => oid.to_string(),
        }
    };
    let zero_id = |oid: &Option<ObjectId>| -> String {
        match oid {
            Some(oid) => show(oid),
            None => "0".repeat(minimum.unwrap_or(format.hex_len())),
        }
    };
    let end = if opts.nul { b'\0' } else { b'\n' };
    let named_only = opts.name_only || opts.name_status;
    let text_mode = (opts.patch || opts.stat || opts.numstat) && !named_only;
    let want_raw = !text_mode || opts.raw;
    let render = |jobs: Vec<Job>| -> Result<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        for (header, old, new) in jobs {
            let mut body: Vec<u8> = Vec::new();
            let mut any = false;
            if want_raw {
                let mut walker = Walker {
                    repository: &repository,
                    recursive: opts.recursive || text_mode,
                    show_trees: opts.show_trees,
                    specs: &specs,
                    out: Vec::new(),
                };
                walker.walk(old, Some(new), BStr::new(b""))?;
                if opts.rename_score.is_some() {
                    pair_records(&repository, &opts, old, &specs, &mut walker.out)?;
                }
                any = !walker.out.is_empty();
                for rec in walker.out.iter().filter(|_| !opts.silent) {
                    let path = if opts.nul {
                        rec.path.to_vec()
                    } else {
                        rivet_core::ops::quote_path(rec.path.as_bstr(), false).to_vec()
                    };
                    if opts.name_only {
                        body.extend_from_slice(&path);
                    } else if opts.name_status {
                        body.push(rec.status);
                        body.extend_from_slice(&score_digits(rec));
                        body.push(if opts.nul { b'\0' } else { b'\t' });
                        body.extend_from_slice(&old_path_field(rec, &opts));
                        body.extend_from_slice(&path);
                    } else {
                        let mode = |mode: Option<EntryMode>| mode.map_or(0, EntryMode::to_u32);
                        body.extend_from_slice(
                            format!(
                                ":{:06o} {:06o} {} {} {}",
                                mode(rec.omode),
                                mode(rec.nmode),
                                zero_id(&rec.ooid),
                                zero_id(&rec.noid),
                                rec.status as char,
                            )
                            .as_bytes(),
                        );
                        body.extend_from_slice(&score_digits(rec));
                        body.push(if opts.nul { b'\0' } else { b'\t' });
                        body.extend_from_slice(&old_path_field(rec, &opts));
                        body.extend_from_slice(&path);
                    }
                    body.push(end);
                }
            }
            if text_mode {
                let empty = rivet_core::object::hash_object(format, ObjectKind::Tree, b"")?.0;
                let mut options = rivet_core::DiffOptions::new();
                options.detect_binary = true;
                let mut diff = rivet_core::tree_diff(
                    format,
                    old.unwrap_or(empty),
                    new,
                    repository.odb(),
                    &options,
                )?;
                if !specs.is_empty() {
                    let keep: Vec<bool> = diff
                        .files
                        .iter()
                        .map(|file| {
                            specs.iter().any(|spec| {
                                rivet_core::ops::pathspec_matches(file.path(), spec.as_bstr())
                            })
                        })
                        .collect();
                    let mut at = 0;
                    diff.files.retain(|_| {
                        at += 1;
                        keep[at - 1]
                    });
                    let mut at = 0;
                    diff.stats.retain(|_| {
                        at += 1;
                        keep[at - 1]
                    });
                }
                let copied = if opts.rename_score.is_some() {
                    pair_files(&repository, &opts, old, &specs, &mut diff)?
                } else {
                    Vec::new()
                };
                any = !diff.is_empty();
                if any && !opts.silent {
                    if opts.numstat {
                        for (stat, file) in diff.stats.iter().zip(&diff.files) {
                            if stat.is_binary {
                                body.extend_from_slice(b"-\t-\t");
                            } else {
                                body.extend_from_slice(
                                    format!("{}\t{}\t", stat.added, stat.deleted).as_bytes(),
                                );
                            }
                            match (&file.old_path, file.is_rename(), opts.nul) {
                                (Some(old), true, true) => {
                                    body.push(b'\0');
                                    body.extend_from_slice(old);
                                    body.push(b'\0');
                                    body.extend_from_slice(file.path());
                                }
                                (Some(old), true, false) => {
                                    let (qo, qn) = (
                                        rivet_core::ops::quote_path(old.as_bstr(), false),
                                        rivet_core::ops::quote_path(file.path(), false),
                                    );
                                    if qo.as_slice() != old.as_slice()
                                        || qn.as_slice() != file.path()
                                    {
                                        body.extend_from_slice(&qo);
                                        body.extend_from_slice(b" => ");
                                        body.extend_from_slice(&qn);
                                    } else {
                                        body.extend_from_slice(&rivet_core::pprint_rename(
                                            old,
                                            file.path(),
                                        ));
                                    }
                                }
                                (_, _, true) => body.extend_from_slice(&stat.path),
                                (_, _, false) => body.extend_from_slice(
                                    &rivet_core::ops::quote_path(stat.path.as_bstr(), false),
                                ),
                            }
                            body.push(end);
                        }
                    }
                    if opts.stat {
                        rivet_core::format_stat_sized(&diff, Some(repository.odb()), &mut body)?;
                    }
                    if opts.patch {
                        // The blank line between the other formats and the patch ends in NUL under -z.
                        if !body.is_empty() {
                            body.push(end);
                        }
                        let mut patch = Vec::new();
                        rivet_core::format_unified(&diff, format, &mut patch)?;
                        if opts.full_index {
                            patch = widen_index_lines(&patch, &diff, format);
                        }
                        if !copied.is_empty() {
                            patch = name_copies(&patch, &copied);
                        }
                        body.extend_from_slice(&patch);
                    }
                }
            }
            if !any {
                continue;
            }
            if let Some(id) = header
                && !opts.no_commit_id
            {
                out.extend_from_slice(&id);
                out.push(end);
            }
            out.extend_from_slice(&body);
        }
        Ok(out)
    };
    write_stdout(&render(jobs)?)?;

    if opts.stdin {
        use std::io::Read as _;
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input)?;
        let mut lines: Vec<&[u8]> = input.split(|b| *b == b'\n').collect();
        // What follows the last newline is a line git does not act on (it echoes it if it does not
        // start with an id).
        let unterminated = lines.pop().unwrap_or_default();
        if !unterminated.is_empty() {
            let hex = format.hex_len();
            let starts_with_id = unterminated.len() >= hex
                && ObjectId::from_hex(format, BStr::new(&unterminated[..hex])).is_ok();
            if !starts_with_id {
                lines.push(unterminated);
            }
        }
        let last = lines.len().saturating_sub(1);
        for (at, line) in lines.into_iter().enumerate() {
            let tail = at == last && !unterminated.is_empty() && line == unterminated;
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let mut ids: Vec<ObjectId> = Vec::new();
            for word in line.split(|b| *b == b' ') {
                match ObjectId::from_hex(format, BStr::new(word)) {
                    Ok(id) => ids.push(id),
                    Err(_) => break,
                }
            }
            let echo = || write_stdout(&[line, if tail { b"" } else { b"\n" }].concat());
            if ids.is_empty() {
                echo()?;
                continue;
            }
            let Ok(object) = repository.odb().read(&ids[0]) else {
                continue;
            };
            let mut jobs: Vec<Job> = Vec::new();
            match object.kind() {
                ObjectKind::Commit => {
                    let commit = Commit::parse(format, object.data())?;
                    if ids.len() == 1 {
                        let parents = repository.odb().visible_parents(&ids[0], &commit);
                        jobs = commit_jobs(&ids[0], &commit.tree, &parents)?;
                    } else if ids.len() == 2 || opts.merges {
                        for parent in &ids[1..] {
                            let parent_tree = tree_of(&name(&repository, &parent.to_string())?)?;
                            jobs.push((
                                Some(ids[0].to_string().into_bytes()),
                                Some(parent_tree),
                                commit.tree,
                            ));
                        }
                    }
                }
                ObjectKind::Tree => {
                    if ids.len() != 2 {
                        eprintln!("error: Need exactly two trees, separated by a space");
                        continue;
                    }
                    if repository
                        .odb()
                        .read_typed(&ids[1], ObjectKind::Tree)
                        .is_err()
                    {
                        eprintln!("error: Object {} not a tree", ids[1]);
                        continue;
                    }
                    let second = ids[1];
                    jobs.push((
                        Some(format!("{} {}", ids[0], ids[1]).into_bytes()),
                        Some(ids[0]),
                        second,
                    ));
                }
                other => {
                    eprintln!(
                        "error: Object {} is a {}, not a commit or tree",
                        ids[0],
                        other.as_str()
                    );
                    continue;
                }
            }
            write_stdout(&render(jobs)?)?;
        }
    }
    Ok(0)
}

/// One comparison: the header line (a commit id, or two tree ids), the old tree and the new tree.
type Job = (Option<Vec<u8>>, Option<ObjectId>, ObjectId);

/// `--full-index`: every `index <old>..<new>` line of `patch` with the ids spelled out in full.
fn widen_index_lines(
    patch: &[u8],
    diff: &rivet_core::TreeDiff,
    format: rivet_core::ObjectFormat,
) -> Vec<u8> {
    let zeros = "0".repeat(format.hex_len());
    let full = |oid: &Option<ObjectId>| match oid {
        Some(oid) => oid.to_string(),
        None => zeros.clone(),
    };
    let matches = |oid: &Option<ObjectId>, short: &str| match oid {
        Some(oid) => oid.to_string().starts_with(short),
        None => short.bytes().all(|b| b == b'0'),
    };
    let mut out = Vec::with_capacity(patch.len());
    for line in patch.split_inclusive(|b| *b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"index ")
            && let Ok(rest) = std::str::from_utf8(rest)
        {
            let cut = rest.find([' ', '\n']).unwrap_or(rest.len());
            if let Some((old, new)) = rest[..cut].split_once("..")
                && let Some(file) = diff
                    .files
                    .iter()
                    .flat_map(|file| match &file.split {
                        Some(split) => vec![&split.0, &split.1],
                        None => vec![file],
                    })
                    .find(|file| matches(&file.old_oid, old) && matches(&file.new_oid, new))
            {
                out.extend_from_slice(
                    format!(
                        "index {}..{}{}",
                        full(&file.old_oid),
                        full(&file.new_oid),
                        &rest[cut..]
                    )
                    .as_bytes(),
                );
                continue;
            }
        }
        out.extend_from_slice(line);
    }
    out
}

/// The score digits after the status letter of a rename or copy (`R086`).
fn score_digits(rec: &Rec) -> Vec<u8> {
    rec.score
        .map(|score| format!("{score:03}").into_bytes())
        .unwrap_or_default()
}

/// The source path field before the path of a rename or copy, with its separator.
fn old_path_field(rec: &Rec, opts: &Opts) -> Vec<u8> {
    let Some(old) = &rec.old_path else {
        return Vec::new();
    };
    let mut field = if opts.nul {
        old.to_vec()
    } else {
        rivet_core::ops::quote_path(old.as_bstr(), false).to_vec()
    };
    field.push(if opts.nul { b'\0' } else { b'\t' });
    field
}

/// Every blob of `tree` under `prefix`: the sources `--find-copies-harder` looks at.
fn all_blobs(
    repository: &Repository,
    tree: ObjectId,
    prefix: &[u8],
    out: &mut Vec<rivet_core::RenameSide>,
) -> Result<()> {
    let data = repository
        .odb()
        .read_typed(&tree, ObjectKind::Tree)?
        .into_data();
    for entry in Tree::parse(repository.format(), &data)?.entries {
        let mut path = prefix.to_vec();
        path.extend_from_slice(&entry.name);
        if entry.mode == EntryMode::Tree {
            path.push(b'/');
            all_blobs(repository, entry.oid, &path, out)?;
        } else if entry.mode != EntryMode::Commit {
            out.push(rivet_core::RenameSide {
                path: BString::from(path),
                oid: entry.oid,
                mode: entry.mode,
            });
        }
    }
    Ok(())
}

/// Which source serves which destination, as `(source, destination, score, is_copy)` in
/// destination order. `deleted` says whether a source is a deleted file (the only kind a rename
/// may take over; any other source can only be copied from).
fn choose_pairs(
    repository: &Repository,
    opts: &Opts,
    sources: &[rivet_core::RenameSide],
    deleted: &[bool],
    destinations: &[rivet_core::RenameSide],
) -> Result<Vec<(usize, usize, u64, bool)>> {
    let minimum = opts
        .rename_score
        .unwrap_or(rivet_core::diff::rename::MINIMUM_SCORE);
    let pairs = if opts.copies {
        rivet_core::diff::pair_copies(sources, destinations, repository.odb(), minimum)?
    } else {
        // Renames only: the deleted files are the sources, each used once.
        let at: Vec<usize> = (0..sources.len()).filter(|i| deleted[*i]).collect();
        let only: Vec<rivet_core::RenameSide> = at.iter().map(|i| sources[*i].clone()).collect();
        let mut pairs =
            rivet_core::diff::pair_renames_at(&only, destinations, repository.odb(), minimum)?;
        for pair in &mut pairs {
            pair.source = at[pair.source];
        }
        pairs
    };
    // A deleted file several destinations come from is renamed to the last of them and copied
    // to the others; a source that stays is only ever copied.
    let mut uses = vec![0usize; sources.len()];
    for pair in &pairs {
        uses[pair.source] += 1;
    }
    let mut chosen = Vec::new();
    for pair in pairs {
        uses[pair.source] -= 1;
        let copy = !deleted[pair.source] || uses[pair.source] > 0;
        chosen.push((pair.source, pair.destination, pair.score, copy));
    }
    Ok(chosen)
}

/// Turn the added files of `recs` that match a deleted (or, with `-C`, modified) one into
/// renames and copies, in place.
fn pair_records(
    repository: &Repository,
    opts: &Opts,
    old: Option<ObjectId>,
    specs: &[BString],
    recs: &mut Vec<Rec>,
) -> Result<()> {
    let mut sources: Vec<rivet_core::RenameSide> = Vec::new();
    let mut source_rec: Vec<Option<usize>> = Vec::new();
    let mut deleted: Vec<bool> = Vec::new();
    let mut destinations: Vec<rivet_core::RenameSide> = Vec::new();
    let mut destination_rec: Vec<usize> = Vec::new();
    for (index, rec) in recs.iter().enumerate() {
        match (rec.status, rec.ooid, rec.omode, rec.noid, rec.nmode) {
            (b'A', _, _, Some(oid), Some(mode)) if mode != EntryMode::Tree => {
                destinations.push(rivet_core::RenameSide {
                    path: rec.path.clone(),
                    oid,
                    mode,
                });
                destination_rec.push(index);
            }
            (b'D', Some(oid), Some(mode), _, _) if mode != EntryMode::Tree => {
                sources.push(rivet_core::RenameSide {
                    path: rec.path.clone(),
                    oid,
                    mode,
                });
                source_rec.push(Some(index));
                deleted.push(true);
            }
            (b'M' | b'T', Some(oid), Some(mode), _, _)
                if opts.copies && mode != EntryMode::Tree =>
            {
                sources.push(rivet_core::RenameSide {
                    path: rec.path.clone(),
                    oid,
                    mode,
                });
                source_rec.push(Some(index));
                deleted.push(false);
            }
            _ => {}
        }
    }
    if opts.harder
        && let Some(tree) = old
    {
        let mut everything = Vec::new();
        all_blobs(repository, tree, b"", &mut everything)?;
        let known: std::collections::HashMap<BString, usize> = sources
            .iter()
            .enumerate()
            .map(|(at, side)| (side.path.clone(), at))
            .collect();
        for side in everything {
            let wanted = specs.is_empty()
                || specs.iter().any(|spec| {
                    rivet_core::ops::pathspec_matches(side.path.as_bstr(), spec.as_bstr())
                });
            if wanted && !known.contains_key(&side.path) {
                sources.push(side);
                source_rec.push(None);
                deleted.push(false);
            }
        }
    }
    let chosen = choose_pairs(repository, opts, &sources, &deleted, &destinations)?;
    let mut remove: Vec<usize> = Vec::new();
    for (source, destination, score, copy) in chosen {
        let rec = &mut recs[destination_rec[destination]];
        rec.omode = Some(sources[source].mode);
        rec.ooid = Some(sources[source].oid);
        rec.old_path = Some(sources[source].path.clone());
        rec.score = Some((score * 100 / rivet_core::diff::MAX_SCORE) as u32);
        rec.status = if copy { b'C' } else { b'R' };
        if !copy && let Some(at) = source_rec[source] {
            remove.push(at);
        }
    }
    remove.sort_unstable();
    for at in remove.into_iter().rev() {
        recs.remove(at);
    }
    Ok(())
}

/// The same pairing on a computed diff: renamed files become one entry with two paths, copies
/// keep their source. Returns the paths that are copies.
fn pair_files(
    repository: &Repository,
    opts: &Opts,
    old: Option<ObjectId>,
    specs: &[BString],
    diff: &mut rivet_core::TreeDiff,
) -> Result<Vec<BString>> {
    let mut sources: Vec<rivet_core::RenameSide> = Vec::new();
    let mut source_file: Vec<Option<usize>> = Vec::new();
    let mut deleted: Vec<bool> = Vec::new();
    let mut destinations: Vec<rivet_core::RenameSide> = Vec::new();
    let mut destination_file: Vec<usize> = Vec::new();
    for (index, file) in diff.files.iter().enumerate() {
        if file.is_new
            && let (Some(oid), Some(mode)) = (file.new_oid, file.new_mode)
        {
            destinations.push(rivet_core::RenameSide {
                path: file.path().to_owned(),
                oid,
                mode,
            });
            destination_file.push(index);
        } else if let (Some(oid), Some(mode)) = (file.old_oid, file.old_mode)
            && (file.is_deleted || opts.copies)
        {
            sources.push(rivet_core::RenameSide {
                path: file.path().to_owned(),
                oid,
                mode,
            });
            source_file.push(Some(index));
            deleted.push(file.is_deleted);
        }
    }
    if opts.harder
        && let Some(tree) = old
    {
        let mut everything = Vec::new();
        all_blobs(repository, tree, b"", &mut everything)?;
        let known: std::collections::HashSet<BString> =
            sources.iter().map(|side| side.path.clone()).collect();
        for side in everything {
            let wanted = specs.is_empty()
                || specs.iter().any(|spec| {
                    rivet_core::ops::pathspec_matches(side.path.as_bstr(), spec.as_bstr())
                });
            if wanted && !known.contains(&side.path) {
                sources.push(side);
                source_file.push(None);
                deleted.push(false);
            }
        }
    }
    let chosen = choose_pairs(repository, opts, &sources, &deleted, &destinations)?;
    let mut options = rivet_core::DiffOptions::new();
    options.detect_binary = true;
    let mut remove: Vec<usize> = Vec::new();
    let mut copies: Vec<BString> = Vec::new();
    for (source, destination, score, copy) in chosen {
        let (from, to) = (&sources[source], &destinations[destination]);
        let mut paired = rivet_core::blob_diff(
            repository.format(),
            BStr::new(&to.path[..]),
            Some((from.oid, from.mode)),
            Some((to.oid, to.mode)),
            repository.odb(),
            &options,
        )?;
        paired.old_path = Some(from.path.clone());
        paired.new_path = Some(to.path.clone());
        paired.is_new = false;
        paired.is_deleted = false;
        paired.similarity = Some((score * 100 / rivet_core::diff::MAX_SCORE) as u32);
        diff.files[destination_file[destination]] = paired;
        if copy {
            copies.push(to.path.clone());
        } else if let Some(at) = source_file[source] {
            remove.push(at);
        }
    }
    remove.sort_unstable();
    for at in remove.into_iter().rev() {
        diff.files.remove(at);
    }
    diff.stats = diff.files.iter().map(rivet_core::FileDiff::stat).collect();
    Ok(copies)
}

/// A patch's `rename from` / `rename to` headers of the files in `copies`, spelled `copy from` /
/// `copy to` (the formatter knows only renames).
fn name_copies(patch: &[u8], copies: &[BString]) -> Vec<u8> {
    let targets: Vec<Vec<u8>> = copies
        .iter()
        .map(|path| {
            let mut line = b"rename to ".to_vec();
            line.extend_from_slice(&rivet_core::ops::quote_path(path.as_bstr(), false));
            line.push(b'\n');
            line
        })
        .collect();
    let lines: Vec<&[u8]> = patch.split_inclusive(|b| *b == b'\n').collect();
    let mut out = Vec::with_capacity(patch.len());
    for (at, line) in lines.iter().enumerate() {
        let is_copy_to = targets.iter().any(|target| target == line);
        let is_copy_from = line.starts_with(b"rename from ")
            && lines
                .get(at + 1)
                .is_some_and(|next| targets.iter().any(|target| target == next));
        if is_copy_to {
            out.extend_from_slice(b"copy to ");
            out.extend_from_slice(&line[b"rename to ".len()..]);
        } else if is_copy_from {
            out.extend_from_slice(b"copy from ");
            out.extend_from_slice(&line[b"rename from ".len()..]);
        } else {
            out.extend_from_slice(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_score;

    #[test]
    fn scores_are_percentages_or_the_digits_of_a_fraction() {
        let max = rivet_core::diff::MAX_SCORE;
        assert_eq!(parse_score("50%"), Some(max / 2));
        assert_eq!(parse_score("5"), Some(max / 2));
        assert_eq!(parse_score("50"), Some(max / 2));
        assert_eq!(parse_score("05"), Some(max / 20));
        assert_eq!(parse_score("100%"), Some(max));
        assert_eq!(parse_score("0%"), Some(0));
        assert_eq!(parse_score("12.5%"), Some(max * 125 / 1000));
        for bad in ["", "%", "x", "5x", "-1", "1%%"] {
            assert_eq!(parse_score(bad), None, "{bad:?}");
        }
    }
}
