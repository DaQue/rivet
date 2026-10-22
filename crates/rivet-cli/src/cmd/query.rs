//! Query commands scripts lean on: `rev-list`, `show-ref`, `for-each-ref`, `merge-base`,
//! `describe` and `shortlog`.
//!
//! Each one reads the same objects and refs `log` and `tag` read and prints in Git's formats; none
//! of them writes anything. The behaviours that are easy to get subtly wrong — `describe`'s tag
//! precedence and candidate limit, `show-ref`'s tail-matching patterns, `for-each-ref`'s
//! `:short` disambiguation — follow Git's rules and are covered by differential scenarios.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;

use anyhow::Result;
use bstr::ByteSlice;
use rivet_core::object::{Commit, Tag, Tree};
use rivet_core::revwalk::{SortOrder, WalkOptions};
use rivet_core::{ObjectId, ObjectKind, RefTarget, Repository};

use crate::cli::GlobalArgs;

fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|_| stdout.flush()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Into::into),
    }
}

/// Every ref under `refs/` with the object it ends at, sorted by name.
fn refs_sorted(repository: &Repository) -> Result<Vec<(String, ObjectId)>> {
    let mut out = Vec::new();
    for (name, target) in repository.refs().list(b"refs/")? {
        let oid = match target {
            RefTarget::Direct(oid) => oid,
            RefTarget::Symbolic(_) => match repository.refs().resolve_oid(&name) {
                Ok(oid) => oid,
                Err(_) => continue,
            },
        };
        out.push((String::from_utf8_lossy(name.as_bytes()).into_owned(), oid));
    }
    out.sort();
    Ok(out)
}

struct CommitFacts {
    parents: Vec<ObjectId>,
    tree: ObjectId,
}

fn commit_facts(repository: &Repository, oid: &ObjectId) -> Result<CommitFacts> {
    let object = repository.odb().read_typed(oid, ObjectKind::Commit)?;
    let commit = Commit::parse(repository.format(), object.data())?;
    Ok(CommitFacts {
        parents: commit.parents.clone(),
        tree: commit.tree,
    })
}

fn ambiguous(spec: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "ambiguous argument '{spec}': unknown revision or path not in the working tree.\nUse '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
    )
}

fn not_valid(spec: &str) -> anyhow::Error {
    anyhow::anyhow!("Not a valid object name {spec}")
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

// ---------------------------------------------------------------------------------------------
// rev-list
// ---------------------------------------------------------------------------------------------

/// Arguments of `rev-list`.
#[derive(Debug, clap::Args)]
pub struct RevListArgs {
    /// Revisions: `A`, `^A`, `A..B`, `A...B`.
    #[arg(value_name = "REV")]
    pub revs: Vec<String>,

    /// Start from every ref.
    #[arg(long = "all")]
    pub all: bool,

    /// Print only the number of commits listed.
    #[arg(long = "count")]
    pub count: bool,

    /// Stop after this many commits.
    #[arg(short = 'n', long = "max-count", value_name = "N")]
    pub max_count: Option<usize>,

    /// Print the commits in reverse order (after `--max-count` is applied).
    #[arg(long = "reverse")]
    pub reverse: bool,

    /// Also list the trees, blobs and tags the commits reach.
    #[arg(long = "objects")]
    pub objects: bool,

    /// Follow only the first parent of each commit.
    #[arg(long = "first-parent")]
    pub first_parent: bool,
}

fn mark_tree(repository: &Repository, tree: ObjectId, seen: &mut HashSet<ObjectId>) -> Result<()> {
    if !seen.insert(tree) {
        return Ok(());
    }
    let object = repository.odb().read_typed(&tree, ObjectKind::Tree)?;
    let parsed = Tree::parse(repository.format(), object.data())?;
    for entry in &parsed.entries {
        match entry.mode {
            rivet_core::object::EntryMode::Tree => mark_tree(repository, entry.oid, seen)?,
            rivet_core::object::EntryMode::Commit => {}
            _ => {
                seen.insert(entry.oid);
            }
        }
    }
    Ok(())
}

fn list_tree(
    repository: &Repository,
    tree: ObjectId,
    path: &mut Vec<u8>,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<u8>,
) -> Result<()> {
    if !seen.insert(tree) {
        return Ok(());
    }
    out.extend_from_slice(tree.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(path);
    out.push(b'\n');
    let object = repository.odb().read_typed(&tree, ObjectKind::Tree)?;
    let parsed = Tree::parse(repository.format(), object.data())?;
    for entry in &parsed.entries {
        let base = path.len();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        match entry.mode {
            rivet_core::object::EntryMode::Tree => {
                list_tree(repository, entry.oid, path, seen, out)?
            }
            rivet_core::object::EntryMode::Commit => {}
            _ => {
                if seen.insert(entry.oid) {
                    out.extend_from_slice(entry.oid.to_string().as_bytes());
                    out.push(b' ');
                    out.extend_from_slice(path);
                    out.push(b'\n');
                }
            }
        }
        path.truncate(base);
    }
    Ok(())
}

/// Handler for `rivet rev-list`.
///
/// # Exit codes
///
/// `0` on success; `128` for an unknown revision.
pub fn run_rev_list(global: &GlobalArgs, args: &RevListArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let mut options = WalkOptions::new();
    let mut starts: Vec<ObjectId> = Vec::new();
    let mut pending_tags: Vec<ObjectId> = Vec::new();
    let mut excluded_tips: Vec<ObjectId> = Vec::new();
    let note_tips = |repository: &Repository, oid: ObjectId, tags: &mut Vec<ObjectId>| {
        let mut current = oid;
        while let Ok(object) = repository.odb().read(&current) {
            if object.kind() != ObjectKind::Tag {
                break;
            }
            tags.push(current);
            match Tag::parse(repository.format(), object.data()) {
                Ok(tag) => current = tag.object,
                Err(_) => break,
            }
        }
    };
    let peel = |text: &str| -> Result<ObjectId> { super::resolve_commit(&repository, text) };
    let bad_rev = |result: Result<ObjectId>, spec: &str| -> Result<ObjectId> {
        result.map_err(|_| ambiguous(spec))
    };
    if args.revs.is_empty() && !args.all {
        eprintln!("usage: rivet rev-list [<options>] <commit>...");
        return Ok(129);
    }
    if args.all {
        if let Some(head) = repository.head()?.oid() {
            starts.push(head);
        }
        for (_, oid) in refs_sorted(&repository)? {
            note_tips(&repository, oid, &mut pending_tags);
            if let Ok(commit) = repository.refs().peel_oid(oid, repository.odb())
                && !starts.contains(&commit)
                && repository
                    .odb()
                    .read(&commit)
                    .is_ok_and(|o| o.kind() == ObjectKind::Commit)
            {
                starts.push(commit);
            }
        }
    }
    for spec in &args.revs {
        if let Some(negated) = spec.strip_prefix('^') {
            let oid = bad_rev(peel(negated), negated)?;
            options.exclude(oid);
            excluded_tips.push(oid);
        } else if let Some((left, right)) = spec.split_once("...") {
            let left = peel(if left.is_empty() { "HEAD" } else { left })?;
            let right = peel(if right.is_empty() { "HEAD" } else { right })?;
            starts.push(left);
            starts.push(right);
            for base in rivet_core::merge_bases(repository.odb(), left, right).unwrap_or_default() {
                options.exclude(base);
                excluded_tips.push(base);
            }
        } else if let Some((left, right)) = spec.split_once("..") {
            let left = bad_rev(peel(if left.is_empty() { "HEAD" } else { left }), spec)?;
            options.exclude(left);
            excluded_tips.push(left);
            starts.push(bad_rev(
                peel(if right.is_empty() { "HEAD" } else { right }),
                spec,
            )?);
        } else {
            if let Ok(raw) = super::resolve_rev(&repository, spec) {
                note_tips(&repository, raw, &mut pending_tags);
            }
            starts.push(bad_rev(peel(spec), spec)?);
        }
    }
    options.first_parent(args.first_parent);
    if let Some(n) = args.max_count {
        options.max_count(n);
    }
    options.sort(SortOrder::Date);
    let mut commits = rivet_core::revwalk::walk_oids(repository.odb(), &starts, &options)?;
    if args.reverse {
        commits.reverse();
    }

    let mut out = Vec::new();
    let mut lines = 0usize;
    for oid in &commits {
        out.extend_from_slice(oid.to_string().as_bytes());
        out.push(b'\n');
        lines += 1;
    }
    if args.objects {
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let uninteresting: HashSet<ObjectId> = if excluded_tips.is_empty() {
            HashSet::new()
        } else {
            rivet_core::revwalk::walk_oids(repository.odb(), &excluded_tips, &WalkOptions::new())?
                .into_iter()
                .collect()
        };
        // Trees of the uninteresting commits the listed ones sit on are not listed again.
        for oid in &commits {
            for parent in commit_facts(&repository, oid)?.parents {
                if uninteresting.contains(&parent) {
                    mark_tree(
                        &repository,
                        commit_facts(&repository, &parent)?.tree,
                        &mut seen,
                    )?;
                }
            }
        }
        let mut objects = Vec::new();
        for tag in &pending_tags {
            if seen.insert(*tag) {
                let object = repository.odb().read(tag)?;
                let parsed = Tag::parse(repository.format(), object.data())?;
                objects.extend_from_slice(tag.to_string().as_bytes());
                objects.push(b' ');
                objects.extend_from_slice(parsed.name);
                objects.push(b'\n');
            }
        }
        let order = commits.clone();
        for oid in &order {
            let tree = commit_facts(&repository, oid)?.tree;
            list_tree(&repository, tree, &mut Vec::new(), &mut seen, &mut objects)?;
        }
        lines += objects.iter().filter(|b| **b == b'\n').count();
        out.extend_from_slice(&objects);
    }
    if args.count {
        write_stdout(format!("{lines}\n").as_bytes())?;
    } else {
        write_stdout(&out)?;
    }
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// show-ref
// ---------------------------------------------------------------------------------------------

/// Arguments of `show-ref`.
#[derive(Debug, clap::Args)]
pub struct ShowRefArgs {
    /// Patterns: a ref matches when it ends with the pattern at a `/` boundary.
    #[arg(value_name = "PATTERN")]
    pub patterns: Vec<String>,

    /// Show only `refs/heads/`.
    #[arg(long = "heads")]
    pub heads: bool,

    /// Show only `refs/tags/`.
    #[arg(long = "tags")]
    pub tags: bool,

    /// Also show the peeled value of annotated tags (`refname^{}`).
    #[arg(short = 'd', long = "dereference")]
    pub dereference: bool,

    /// Require exact ref names and fail for any that does not exist.
    #[arg(long = "verify")]
    pub verify: bool,

    /// Include `HEAD`.
    #[arg(long = "head")]
    pub head: bool,

    /// Print only the object names.
    #[arg(short = 's', long = "hash")]
    pub hash: bool,
}

/// Handler for `rivet show-ref`.
///
/// # Exit codes
///
/// `0` when something was shown; `1` when no ref matched; `128` for `--verify` of a missing ref.
pub fn run_show_ref(global: &GlobalArgs, args: &ShowRefArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let refs = refs_sorted(&repository)?;
    let mut candidates: Vec<(String, ObjectId)> = Vec::new();
    if (args.head || (args.verify && args.patterns.iter().any(|p| p == "HEAD")))
        && let Some(oid) = repository.head()?.oid()
    {
        candidates.push(("HEAD".to_string(), oid));
    }
    let category = args.heads || args.tags;
    for (name, oid) in &refs {
        if category
            && !((args.heads && name.starts_with("refs/heads/"))
                || (args.tags && name.starts_with("refs/tags/")))
        {
            continue;
        }
        candidates.push((name.clone(), *oid));
    }
    let mut out = String::new();
    let emit = |repository: &Repository, name: &str, oid: ObjectId, out: &mut String| {
        if args.hash {
            out.push_str(&format!("{oid}\n"));
        } else {
            out.push_str(&format!("{oid} {name}\n"));
        }
        if args.dereference
            && let Ok(object) = repository.odb().read(&oid)
            && object.kind() == ObjectKind::Tag
            && let Ok(peeled) = repository.refs().peel_oid(oid, repository.odb())
        {
            // With `-s` Git drops the name from the first line but keeps it on this one.
            out.push_str(&format!("{peeled} {name}^{{}}\n"));
        }
    };
    if args.verify {
        if args.patterns.is_empty() {
            eprintln!("fatal: --verify requires a reference");
            return Ok(128);
        }
        for pattern in &args.patterns {
            let found = candidates.iter().find(|(name, _)| name == pattern);
            match found {
                Some((name, oid)) if pattern.starts_with("refs/") || pattern == "HEAD" => {
                    emit(&repository, name, *oid, &mut out);
                }
                _ => {
                    if global.quiet {
                        return Ok(1);
                    }
                    eprintln!("fatal: '{pattern}' - not a valid ref");
                    return Ok(128);
                }
            }
        }
        if !global.quiet {
            write_stdout(out.as_bytes())?;
        }
        return Ok(0);
    }
    let mut shown = 0;
    for (name, oid) in &candidates {
        let matches = args.patterns.is_empty()
            || args
                .patterns
                .iter()
                .any(|p| name == p || name.ends_with(&format!("/{p}")));
        if matches {
            emit(&repository, name, *oid, &mut out);
            shown += 1;
        }
    }
    if !global.quiet {
        write_stdout(out.as_bytes())?;
    }
    Ok(if shown == 0 { 1 } else { 0 })
}

// ---------------------------------------------------------------------------------------------
// for-each-ref
// ---------------------------------------------------------------------------------------------

/// Arguments of `for-each-ref`.
#[derive(Debug, clap::Args)]
pub struct ForEachRefArgs {
    /// Patterns: a ref prefix, or a glob.
    #[arg(value_name = "PATTERN")]
    pub patterns: Vec<String>,

    /// The output format, with `%(atom)` placeholders.
    #[arg(long = "format", value_name = "FORMAT")]
    pub format: Option<String>,

    /// Show at most this many refs.
    #[arg(long = "count", value_name = "N")]
    pub count: Option<usize>,

    /// Sort key: `refname`, `-refname`, `objectname`, `-objectname`.
    #[arg(long = "sort", value_name = "KEY")]
    pub sort: Vec<String>,
}

/// Git's `shorten_unambiguous_ref` for `refname:short`.
fn shorten(name: &str, all: &HashSet<&str>) -> String {
    // (prefix, suffix) of the rules, most general first.
    let rules: [(&str, &str); 6] = [
        ("", ""),
        ("refs/", ""),
        ("refs/tags/", ""),
        ("refs/heads/", ""),
        ("refs/remotes/", ""),
        ("refs/remotes/", "/HEAD"),
    ];
    for i in (1..rules.len()).rev() {
        let (prefix, suffix) = rules[i];
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        let Some(short) = rest.strip_suffix(suffix) else {
            continue;
        };
        if short.is_empty() {
            continue;
        }
        // Strict mode (`core.warnAmbiguousRefs`, the default): any other rule that names a ref.
        let ambiguous = rules
            .iter()
            .enumerate()
            .any(|(j, (p, s))| j != i && all.contains(format!("{p}{short}{s}").as_str()));
        if !ambiguous {
            return short.to_string();
        }
    }
    name.to_string()
}

fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            glob_match(&pattern[1..], text) || (!text.is_empty() && glob_match(pattern, &text[1..]))
        }
        (Some(b'?'), Some(_)) => glob_match(&pattern[1..], &text[1..]),
        (Some(p), Some(t)) if p == t => glob_match(&pattern[1..], &text[1..]),
        _ => false,
    }
}

fn ref_pattern_matches(pattern: &str, name: &str) -> bool {
    if pattern.contains(['*', '?', '[']) {
        glob_match(pattern.as_bytes(), name.as_bytes())
    } else {
        name == pattern
            || (name.starts_with(pattern)
                && (pattern.ends_with('/') || name.as_bytes().get(pattern.len()) == Some(&b'/')))
    }
}

/// Handler for `rivet for-each-ref`.
///
/// # Exit codes
///
/// `0`; `128` for an unknown field name or sort key.
pub fn run_for_each_ref(global: &GlobalArgs, args: &ForEachRefArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let mut refs = refs_sorted(&repository)?;
    refs.retain(|(name, _)| {
        args.patterns.is_empty() || args.patterns.iter().any(|p| ref_pattern_matches(p, name))
    });
    for key in args.sort.iter().rev() {
        let (descending, field) = match key.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, key.as_str()),
        };
        match field {
            "refname" => refs.sort_by(|a, b| a.0.cmp(&b.0)),
            "objectname" => refs.sort_by_key(|entry| entry.1),
            other => {
                eprintln!("fatal: unknown field name: {other}");
                return Ok(128);
            }
        }
        if descending {
            refs.reverse();
        }
    }
    if let Some(n) = args.count {
        refs.truncate(n);
    }
    let format = args
        .format
        .clone()
        .unwrap_or_else(|| "%(objectname) %(objecttype)\t%(refname)".to_string());

    let all_names: Vec<String> = refs_sorted(&repository)?
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let all: HashSet<&str> = all_names.iter().map(String::as_str).collect();
    let head_ref = repository.head()?.branch().map(|b| b.to_string());
    let mut out = Vec::new();
    for (name, oid) in &refs {
        let object = repository.odb().read(oid)?;
        let mut rest = format.as_str();
        while let Some(at) = rest.find('%') {
            out.extend_from_slice(&rest.as_bytes()[..at]);
            rest = &rest[at + 1..];
            if let Some(after) = rest.strip_prefix('%') {
                out.push(b'%');
                rest = after;
                continue;
            }
            let Some(inner) = rest.strip_prefix('(') else {
                out.push(b'%');
                continue;
            };
            let Some(end) = inner.find(')') else {
                eprintln!("fatal: malformed format string %({inner}");
                return Ok(128);
            };
            let atom = &inner[..end];
            rest = &inner[end + 1..];
            let (peeled, base) = match atom.strip_prefix('*') {
                Some(base) => (true, base),
                None => (false, atom),
            };
            let (field, modifier) = match base.split_once(':') {
                Some((f, m)) => (f, Some(m)),
                None => (base, None),
            };
            let (target_oid, target_kind, target_size) = if peeled {
                if object.kind() == ObjectKind::Tag {
                    let tag = Tag::parse(repository.format(), object.data())?;
                    let peeled_oid = tag.object;
                    let inner_object = repository.odb().read(&peeled_oid)?;
                    (
                        Some(peeled_oid),
                        kind_name(inner_object.kind()),
                        inner_object.data().len(),
                    )
                } else {
                    (None, "", 0)
                }
            } else {
                (Some(*oid), kind_name(object.kind()), object.data().len())
            };
            let text: String = match field {
                "refname" => match modifier {
                    None => name.clone(),
                    Some("short") => shorten(name, &all),
                    Some(m) if m.starts_with("lstrip=") || m.starts_with("strip=") => {
                        let n: usize = m
                            .split_once('=')
                            .and_then(|(_, n)| n.parse().ok())
                            .unwrap_or(0);
                        name.split('/').skip(n).collect::<Vec<_>>().join("/")
                    }
                    Some(m) if m.starts_with("rstrip=") => {
                        let n: usize = m
                            .split_once('=')
                            .and_then(|(_, n)| n.parse().ok())
                            .unwrap_or(0);
                        let parts: Vec<&str> = name.split('/').collect();
                        parts[..parts.len().saturating_sub(n)].join("/")
                    }
                    Some(m) => {
                        eprintln!("fatal: unrecognized %(refname) argument: {m}");
                        return Ok(128);
                    }
                },
                "objectname" => match (target_oid, modifier) {
                    (None, _) => String::new(),
                    (Some(id), None) => id.to_string(),
                    (Some(id), Some("short")) => super::short_oid(&id),
                    (Some(id), Some(m)) if m.starts_with("short=") => {
                        let n: usize = m[6..].parse().unwrap_or(7);
                        id.to_string().chars().take(n.max(4)).collect()
                    }
                    (_, Some(m)) => {
                        eprintln!("fatal: unrecognized %(objectname) argument: {m}");
                        return Ok(128);
                    }
                },
                "objecttype" => target_kind.to_string(),
                "objectsize" => {
                    if target_oid.is_some() {
                        target_size.to_string()
                    } else {
                        String::new()
                    }
                }
                "symref" => {
                    match repository
                        .refs()
                        .list(b"refs/")?
                        .into_iter()
                        .find(|(n, _)| n.as_bytes() == name.as_bytes())
                    {
                        Some((_, RefTarget::Symbolic(target))) => {
                            String::from_utf8_lossy(target.as_bytes()).into_owned()
                        }
                        _ => String::new(),
                    }
                }
                "HEAD" => {
                    if head_ref.as_deref() == Some(name.as_str()) {
                        "*".to_string()
                    } else {
                        " ".to_string()
                    }
                }
                other => {
                    eprintln!("fatal: unknown field name: {other}");
                    return Ok(128);
                }
            };
            out.extend_from_slice(text.as_bytes());
        }
        out.extend_from_slice(rest.as_bytes());
        out.push(b'\n');
    }
    write_stdout(&out)?;
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// merge-base
// ---------------------------------------------------------------------------------------------

/// Arguments of `merge-base`.
#[derive(Debug, clap::Args)]
pub struct MergeBaseArgs {
    /// The commits.
    #[arg(value_name = "COMMIT", num_args = 1..)]
    pub commits: Vec<String>,

    /// Print every best common ancestor.
    #[arg(short = 'a', long = "all")]
    pub all: bool,

    /// Exit 0 if the first commit is an ancestor of the second, 1 if not.
    #[arg(long = "is-ancestor")]
    pub is_ancestor: bool,
}

/// Handler for `rivet merge-base`.
///
/// # Exit codes
///
/// `0` with the base(s) printed; `1` when there is none (or `--is-ancestor` is false); `128` for
/// an unknown commit.
pub fn run_merge_base(global: &GlobalArgs, args: &MergeBaseArgs) -> Result<i32> {
    let repository = super::open(global)?;
    if args.commits.len() < 2 {
        eprintln!(
            "usage: git merge-base [-a | --all] <commit> <commit>...\n   or: git merge-base [-a | --all] --octopus <commit>...\n   or: git merge-base --is-ancestor <commit> <commit>\n   or: git merge-base --independent <commit>...\n   or: git merge-base --fork-point <ref> [<commit>]\n"
        );
        return Ok(129);
    }
    if args.commits.len() != 2 {
        anyhow::bail!("merge-base: more than two commits are not implemented");
    }
    let peel = |spec: &str| super::resolve_commit(&repository, spec).map_err(|_| not_valid(spec));
    let a = peel(&args.commits[0])?;
    let b = peel(&args.commits[1])?;
    if args.is_ancestor {
        return Ok(if repository.is_ancestor(a, b)? { 0 } else { 1 });
    }
    let bases = rivet_core::merge_bases(repository.odb(), a, b).unwrap_or_default();
    if bases.is_empty() {
        return Ok(1);
    }
    let mut out = String::new();
    for base in bases.iter().take(if args.all { usize::MAX } else { 1 }) {
        out.push_str(&format!("{base}\n"));
    }
    write_stdout(out.as_bytes())?;
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------------------------

/// Arguments of `describe`.
#[derive(Debug, clap::Args)]
pub struct DescribeArgs {
    /// The commit to describe (default `HEAD`).
    #[arg(value_name = "COMMIT-ISH")]
    pub commit: Option<String>,

    /// Use any tag, not just annotated ones.
    #[arg(long = "tags")]
    pub tags: bool,

    /// Show an abbreviated object name when no tag describes the commit.
    #[arg(long = "always")]
    pub always: bool,

    /// Always use the long format.
    #[arg(long = "long")]
    pub long: bool,

    /// Use this many digits of the object name.
    #[arg(long = "abbrev", value_name = "N")]
    pub abbrev: Option<usize>,

    /// Fail unless a tag points exactly at the commit.
    #[arg(long = "exact-match")]
    pub exact_match: bool,
}

struct TagName {
    name: String,
    annotated: bool,
    date: i64,
}

/// Handler for `rivet describe`.
///
/// # Exit codes
///
/// `0`; `128` when no tag can describe the commit.
pub fn run_describe(global: &GlobalArgs, args: &DescribeArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let spec = args.commit.as_deref().unwrap_or("HEAD");
    let target = super::resolve_commit(&repository, spec).map_err(|_| not_valid(spec))?;

    // Tag names by the commit they peel to; the best name per commit wins.
    let mut names: HashMap<ObjectId, TagName> = HashMap::new();
    let mut lightweight_commits: HashSet<ObjectId> = HashSet::new();
    for (refname, oid) in refs_sorted(&repository)? {
        let Some(short) = refname.strip_prefix("refs/tags/") else {
            continue;
        };
        let object = repository.odb().read(&oid)?;
        let (annotated, date) = if object.kind() == ObjectKind::Tag {
            let tag = Tag::parse(repository.format(), object.data())?;
            (true, tag.tagger.when)
        } else {
            (false, 0)
        };
        let Ok(commit) = repository.refs().peel_oid(oid, repository.odb()) else {
            continue;
        };
        if repository
            .odb()
            .read(&commit)
            .map(|o| o.kind() != ObjectKind::Commit)
            .unwrap_or(true)
        {
            continue;
        }
        if !annotated {
            lightweight_commits.insert(commit);
        }
        if !annotated && !args.tags {
            continue;
        }
        let candidate = TagName {
            name: short.to_string(),
            annotated,
            date,
        };
        let replace = match names.get(&commit) {
            None => true,
            Some(existing) => {
                if existing.annotated != candidate.annotated {
                    candidate.annotated
                } else {
                    existing.annotated && existing.date < candidate.date
                }
            }
        };
        if replace {
            names.insert(commit, candidate);
        }
    }

    let abbrev_len = args.abbrev.unwrap_or(7);
    let abbreviate = |oid: &ObjectId| -> String {
        if args.abbrev.is_some() {
            oid.to_string().chars().take(abbrev_len.max(4)).collect()
        } else {
            super::history::abbrev(&repository, oid, false)
        }
    };

    if let Some(exact) = names.get(&target) {
        let line = if args.long {
            format!("{}-0-g{}", exact.name, abbreviate(&target))
        } else {
            exact.name.clone()
        };
        write_stdout(format!("{line}\n").as_bytes())?;
        return Ok(0);
    }
    if args.exact_match {
        eprintln!("fatal: no tag exactly matches '{target}'");
        return Ok(128);
    }
    if names.is_empty() && lightweight_commits.is_empty() && !args.always {
        eprintln!("fatal: No names found, cannot describe anything.");
        return Ok(128);
    }

    // Candidates in the order a date-ordered walk reaches them, at most ten.
    let mut options = WalkOptions::new();
    options.sort(SortOrder::Date);
    let walk = rivet_core::revwalk::walk_oids(repository.odb(), &[target], &options)?;
    let candidates: Vec<(usize, ObjectId)> = walk
        .iter()
        .enumerate()
        .filter(|(_, oid)| names.contains_key(*oid))
        .take(10)
        .map(|(i, oid)| (i, *oid))
        .collect();
    let mut best: Option<(usize, usize, ObjectId)> = None;
    for (order, commit) in &candidates {
        let mut options = WalkOptions::new();
        options.exclude(*commit);
        let depth = rivet_core::revwalk::walk_oids(repository.odb(), &[target], &options)?.len();
        if best.is_none_or(|(d, o, _)| (depth, *order) < (d, o)) {
            best = Some((depth, *order, *commit));
        }
    }
    let Some((depth, _, commit)) = best else {
        if args.always {
            write_stdout(format!("{}\n", abbreviate(&target)).as_bytes())?;
            return Ok(0);
        }
        if !args.tags && walk.iter().any(|o| lightweight_commits.contains(o)) {
            eprintln!(
                "fatal: No annotated tags can describe '{target}'.\nHowever, there were unannotated tags: try --tags."
            );
        } else {
            eprintln!(
                "fatal: No tags can describe '{target}'.\nTry --always, or create some tags."
            );
        }
        return Ok(128);
    };
    let name = &names[&commit].name;
    write_stdout(format!("{name}-{depth}-g{}\n", abbreviate(&target)).as_bytes())?;
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// shortlog
// ---------------------------------------------------------------------------------------------

/// Arguments of `shortlog`.
#[derive(Debug, clap::Args)]
pub struct ShortlogArgs {
    /// The revision to summarise (default `HEAD`).
    #[arg(value_name = "REV")]
    pub revision: Option<String>,

    /// Show only the commit count per author.
    #[arg(short = 's', long = "summary")]
    pub summary: bool,

    /// Sort by the number of commits.
    #[arg(short = 'n', long = "numbered")]
    pub numbered: bool,

    /// Show the author's email address.
    #[arg(short = 'e', long = "email")]
    pub email: bool,

    /// Group by committer instead of author.
    #[arg(short = 'c', long = "committer")]
    pub committer: bool,
}

/// Handler for `rivet shortlog`.
///
/// # Exit codes
///
/// `0`; `128` for an unknown revision.
pub fn run_shortlog(global: &GlobalArgs, args: &ShortlogArgs) -> Result<i32> {
    let repository = super::open(global)?;
    let spec = args.revision.as_deref().unwrap_or("HEAD");
    let start = super::resolve_commit(&repository, spec).map_err(|_| ambiguous(spec))?;
    let mut options = WalkOptions::new();
    options.sort(SortOrder::Date);
    let oids = rivet_core::revwalk::walk_oids(repository.odb(), &[start], &options)?;
    let mut groups: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    for oid in &oids {
        let object = repository.odb().read_typed(oid, ObjectKind::Commit)?;
        let commit = Commit::parse(repository.format(), object.data())?;
        let who = if args.committer {
            &commit.committer
        } else {
            &commit.author
        };
        let mut key = who.name.to_vec();
        if args.email {
            key.extend_from_slice(b" <");
            key.extend_from_slice(&who.email);
            key.push(b'>');
        }
        let subject = commit.summary().to_vec();
        groups.entry(key).or_default().push(subject);
    }
    let mut ordered: Vec<(&Vec<u8>, &Vec<Vec<u8>>)> = groups.iter().collect();
    if args.numbered {
        ordered.sort_by_key(|group| std::cmp::Reverse(group.1.len()));
    }
    let mut out = Vec::new();
    for (key, subjects) in ordered {
        if args.summary {
            out.extend_from_slice(format!("{:6}\t", subjects.len()).as_bytes());
            out.extend_from_slice(key);
            out.push(b'\n');
        } else {
            out.extend_from_slice(key);
            out.extend_from_slice(format!(" ({}):\n", subjects.len()).as_bytes());
            for subject in subjects.iter().rev() {
                out.extend_from_slice(b"      ");
                out.extend_from_slice(subject.as_bstr());
                out.push(b'\n');
            }
            out.push(b'\n');
        }
    }
    write_stdout(&out)?;
    Ok(0)
}
