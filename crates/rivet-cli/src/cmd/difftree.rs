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
    patch: bool,
    nul: bool,
    abbrev: Option<Option<usize>>,
}

/// One line of raw output.
struct Rec {
    omode: Option<EntryMode>,
    nmode: Option<EntryMode>,
    ooid: Option<ObjectId>,
    noid: Option<ObjectId>,
    status: u8,
    path: BString,
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
///   `-r`, `-t`, `--root`, `--no-commit-id`, `-m`, `-s`, `-z` and `--abbrev[=<n>]`. `--stat` and
///   `-p` are always recursive, as in Git, and `--name-only` wins over `--name-status` wins over
///   the patch formats.
/// * **`--first-parent` changes nothing**: Git 2.x prints no merge diffs for it without `-m`.
/// * **Not implemented, refused by name:** `-c`/`--cc`, `--stdin`, rename and copy detection,
///   `--numstat`, `--patch-with-raw`, `--full-index`.
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
            "-p" | "-u" | "--patch" => opts.patch = true,
            "-z" => opts.nul = true,
            "--raw" => {}
            "--abbrev" => opts.abbrev = Some(None),
            "-c"
            | "--cc"
            | "--stdin"
            | "-M"
            | "-C"
            | "--numstat"
            | "--patch-with-raw"
            | "--patch-with-stat"
            | "--full-index"
            | "--find-copies-harder" => {
                anyhow::bail!("diff-tree: {word} is not implemented");
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
    if positional.is_empty() {
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

    // (header id, old tree, new tree) per comparison.
    let mut jobs: Vec<(Option<ObjectId>, Option<ObjectId>, ObjectId)> = Vec::new();
    if named.len() == 2 {
        jobs.push((None, Some(tree_of(&named[0])?), tree_of(&named[1])?));
    } else {
        match &named[0] {
            Named::Commit(oid, tree, parents) => {
                if parents.is_empty() {
                    if opts.root {
                        jobs.push((Some(*oid), None, *tree));
                    }
                } else if parents.len() == 1 || opts.merges {
                    for parent in parents {
                        let parent_tree = match name(&repository, &parent.to_string())? {
                            Named::Commit(_, parent_tree, _) => parent_tree,
                            _ => continue,
                        };
                        jobs.push((Some(*oid), Some(parent_tree), *tree));
                    }
                }
            }
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
    let mut out: Vec<u8> = Vec::new();
    let text_mode = opts.patch || opts.stat;
    for (header, old, new) in jobs {
        let mut body: Vec<u8> = Vec::new();
        let any;
        if text_mode && !opts.name_only && !opts.name_status {
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
            any = !diff.is_empty();
            if any && !opts.silent {
                if opts.stat {
                    rivet_core::format_stat_sized(&diff, Some(repository.odb()), &mut body)?;
                }
                if opts.stat && opts.patch {
                    body.push(b'\n');
                }
                if opts.patch {
                    rivet_core::format_unified(&diff, format, &mut body)?;
                }
            }
        } else {
            let mut walker = Walker {
                repository: &repository,
                recursive: opts.recursive,
                show_trees: opts.show_trees,
                specs: &specs,
                out: Vec::new(),
            };
            walker.walk(old, Some(new), BStr::new(b""))?;
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
                    body.push(if opts.nul { b'\0' } else { b'\t' });
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
                    body.push(if opts.nul { b'\0' } else { b'\t' });
                    body.extend_from_slice(&path);
                }
                body.push(end);
            }
        }
        if !any {
            continue;
        }
        if let Some(id) = header
            && !opts.no_commit_id
        {
            out.extend_from_slice(id.to_string().as_bytes());
            out.push(end);
        }
        out.extend_from_slice(&body);
    }
    write_stdout(&out)?;
    Ok(0)
}
