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
pub(super) fn refs_sorted(repository: &Repository) -> Result<Vec<(String, ObjectId)>> {
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

    /// Print each commit's parents after its id.
    #[arg(long = "parents")]
    pub parents: bool,

    /// Skip merge commits.
    #[arg(long = "no-merges")]
    pub no_merges: bool,

    /// Show only merge commits.
    #[arg(long = "merges")]
    pub merges: bool,

    /// Show only commits with at least this many parents.
    #[arg(long = "min-parents", value_name = "N")]
    pub min_parents: Option<usize>,

    /// Show only commits with at most this many parents.
    #[arg(long = "max-parents", value_name = "N")]
    pub max_parents: Option<usize>,

    /// Show only commits made after this date.
    #[arg(long = "since", alias = "after", value_name = "DATE")]
    pub since: Option<String>,

    /// Show only commits made before this date.
    #[arg(long = "until", alias = "before", value_name = "DATE")]
    pub until: Option<String>,

    /// Show only commits whose author matches the pattern (repeatable, any of them).
    #[arg(long = "author", value_name = "PATTERN")]
    pub author: Vec<String>,

    /// Show only commits whose committer matches the pattern (repeatable, any of them).
    #[arg(long = "committer", value_name = "PATTERN")]
    pub committer: Vec<String>,

    /// Show only commits whose message matches the pattern (repeatable, any of them).
    #[arg(long = "grep", value_name = "PATTERN")]
    pub grep: Vec<String>,

    /// Match every `--grep` pattern, not any of them.
    #[arg(long = "all-match")]
    pub all_match: bool,

    /// Show the commits whose message does *not* match `--grep`.
    #[arg(long = "invert-grep")]
    pub invert_grep: bool,

    /// Ignore case in `--author`, `--committer` and `--grep` patterns.
    #[arg(short = 'i', long = "regexp-ignore-case")]
    pub ignore_case: bool,

    /// Patterns are extended regular expressions.
    #[arg(short = 'E', long = "extended-regexp")]
    pub extended: bool,

    /// Patterns are fixed strings.
    #[arg(short = 'F', long = "fixed-strings")]
    pub fixed: bool,

    /// Print abbreviated object names.
    #[arg(long = "abbrev-commit")]
    pub abbrev_commit: bool,

    /// Print full object names.
    #[arg(long = "no-abbrev-commit")]
    pub no_abbrev_commit: bool,

    /// Abbreviate to at least this many digits (`--abbrev-commit` makes it show).
    #[arg(long = "abbrev", value_name = "N", num_args = 0..=1, require_equals = true, default_missing_value = "0")]
    pub abbrev: Option<usize>,

    /// Show commits in the order of the graph, no parent before all its children.
    #[arg(long = "topo-order")]
    pub topo_order: bool,

    /// Like `--topo-order`, but by committer date wherever the graph allows.
    #[arg(long = "date-order")]
    pub date_order: bool,

    /// Like `--topo-order`, but by author date wherever the graph allows.
    #[arg(long = "author-date-order")]
    pub author_date_order: bool,

    /// Also print the commits that bound a range, prefixed with `-`.
    #[arg(long = "boundary")]
    pub boundary: bool,
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
    let mut tips: Vec<(ObjectId, bool)> = Vec::new();
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
        // Git adds the refs in name order first and `HEAD` last; the order only shows in ties.
        for (_, oid) in refs_sorted(&repository)? {
            note_tips(&repository, oid, &mut pending_tags);
            if let Ok(commit) = repository.refs().peel_oid(oid, repository.odb())
                && !tips.contains(&(commit, false))
                && repository
                    .odb()
                    .read(&commit)
                    .is_ok_and(|o| o.kind() == ObjectKind::Commit)
            {
                tips.push((commit, false));
            }
        }
        if let Some(head) = repository.head()?.oid()
            && !tips.contains(&(head, false))
        {
            tips.push((head, false));
        }
    }
    for spec in &args.revs {
        if let Some(negated) = spec.strip_prefix('^') {
            let oid = bad_rev(peel(negated), negated)?;
            tips.push((oid, true));
            excluded_tips.push(oid);
        } else if let Some((left, right)) = spec.split_once("...") {
            let left = peel(if left.is_empty() { "HEAD" } else { left })?;
            let right = peel(if right.is_empty() { "HEAD" } else { right })?;
            tips.push((left, false));
            tips.push((right, false));
            for base in rivet_core::merge_bases(repository.odb(), left, right).unwrap_or_default() {
                tips.push((base, true));
                excluded_tips.push(base);
            }
        } else if let Some((left, right)) = spec.split_once("..") {
            let left = bad_rev(peel(if left.is_empty() { "HEAD" } else { left }), spec)?;
            tips.push((left, true));
            excluded_tips.push(left);
            tips.push((
                bad_rev(peel(if right.is_empty() { "HEAD" } else { right }), spec)?,
                false,
            ));
        } else {
            if let Ok(raw) = super::resolve_rev(&repository, spec) {
                note_tips(&repository, raw, &mut pending_tags);
            }
            tips.push((bad_rev(peel(spec), spec)?, false));
        }
    }
    let mut spec = super::gitwalk::WalkSpec {
        first_parent: args.first_parent,
        ..Default::default()
    };
    spec.tips = tips;
    spec.topo = if args.author_date_order {
        Some(super::gitwalk::TopoOrder::AuthorDate)
    } else if args.date_order {
        Some(super::gitwalk::TopoOrder::CommitDate)
    } else if args.topo_order {
        Some(super::gitwalk::TopoOrder::Graph)
    } else {
        None
    };
    let parse_date = |text: &str| -> Result<i64> {
        rivet_core::ops::parse_approxidate(text)
            .ok_or_else(|| anyhow::anyhow!("invalid date format: {text}"))
    };
    spec.max_age = args.since.as_deref().map(parse_date).transpose()?;
    spec.min_age = args.until.as_deref().map(parse_date).transpose()?;
    let walked = super::gitwalk::walk(&repository, &spec)?;

    let matcher = CommitMatcher::new(args)?;
    let min_parents = args
        .min_parents
        .unwrap_or(0)
        .max(if args.merges { 2 } else { 0 });
    let max_parents = match (args.max_parents, args.no_merges) {
        (Some(n), true) => Some(n.min(1)),
        (None, true) => Some(1),
        (n, false) => n,
    };
    let mut commits: Vec<ObjectId> = Vec::new();
    for oid in &walked.commits {
        let parent_count = walked.parents[oid].len();
        if parent_count < min_parents || max_parents.is_some_and(|max| parent_count > max) {
            continue;
        }
        if !matcher.is_empty() && !matcher.matches(&repository, oid)? {
            continue;
        }
        commits.push(*oid);
        if args.max_count.is_some_and(|n| commits.len() >= n) {
            break;
        }
    }
    if args.max_count == Some(0) {
        commits.clear();
    }
    // `--boundary`, as `get_revision_internal` does it: while a commit is returned its parents that
    // are not shown yet become candidates (once each); when the commits run out, the candidates
    // that never got shown follow, most recently discovered first.
    let mut boundary: Vec<ObjectId> = Vec::new();
    if args.boundary {
        let mut shown: HashSet<ObjectId> = HashSet::new();
        let mut candidates: Vec<ObjectId> = Vec::new();
        let mut child_shown: HashSet<ObjectId> = HashSet::new();
        for oid in &commits {
            shown.insert(*oid);
            for parent in commit_facts(&repository, oid)?.parents {
                if shown.contains(&parent) || !child_shown.insert(parent) {
                    continue;
                }
                candidates.push(parent);
            }
        }
        // The list is built by pushing each candidate on the front, then sorted topologically
        // with the walk's own order (the graph order when none was asked for).
        let pending: Vec<ObjectId> = candidates
            .into_iter()
            .rev()
            .filter(|oid| !shown.contains(oid))
            .collect();
        boundary = super::gitwalk::sort_topologically(
            &repository,
            pending,
            spec.topo.unwrap_or(super::gitwalk::TopoOrder::Graph),
            &walked.parsed,
        )?;
    }
    commits.extend(boundary.iter().copied());
    if args.reverse {
        commits.reverse();
    }
    let boundary: HashSet<ObjectId> = boundary.into_iter().collect();

    let abbreviate = args.abbrev_commit && !args.no_abbrev_commit;
    let abbreviator = if abbreviate {
        Some(Abbreviator::new(&repository, args.abbrev)?)
    } else {
        None
    };
    let name = |oid: &ObjectId| -> String {
        match &abbreviator {
            Some(abbreviator) => abbreviator.abbreviate(oid),
            None => oid.to_string(),
        }
    };
    let mut out = Vec::new();
    let mut lines = 0usize;
    for oid in &commits {
        if boundary.contains(oid) {
            out.push(b'-');
        }
        out.extend_from_slice(name(oid).as_bytes());
        if args.parents {
            // Parents are listed in full, all of them, whatever `--first-parent` walked.
            for parent in commit_facts(&repository, oid)?.parents {
                out.push(b' ');
                out.extend_from_slice(parent.to_string().as_bytes());
            }
        }
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

/// `--author` / `--committer` / `--grep` filtering of rev-list, with git's grouping: a commit
/// passes when each non-empty group has a matching pattern (every pattern with `--all-match`).
struct CommitMatcher {
    authors: Vec<super::regex::Regex>,
    committers: Vec<super::regex::Regex>,
    greps: Vec<super::regex::Regex>,
    all_match: bool,
    invert: bool,
}

impl CommitMatcher {
    fn new(args: &RevListArgs) -> Result<Self> {
        let compile = |patterns: &[String]| -> Result<Vec<super::regex::Regex>> {
            patterns
                .iter()
                .map(|pattern| {
                    if args.fixed {
                        Ok(super::regex::Regex::literal(
                            pattern.as_bytes(),
                            args.ignore_case,
                        ))
                    } else {
                        super::regex::Regex::new(
                            pattern.as_bytes(),
                            args.extended,
                            args.ignore_case,
                        )
                    }
                })
                .collect()
        };
        Ok(CommitMatcher {
            authors: compile(&args.author)?,
            committers: compile(&args.committer)?,
            greps: compile(&args.grep)?,
            all_match: args.all_match,
            invert: args.invert_grep,
        })
    }

    fn is_empty(&self) -> bool {
        self.authors.is_empty() && self.committers.is_empty() && self.greps.is_empty()
    }

    fn matches(&self, repository: &Repository, oid: &ObjectId) -> Result<bool> {
        let object = repository.odb().read_typed(oid, ObjectKind::Commit)?;
        let commit = Commit::parse(repository.format(), object.data())?;
        let ident = |who: &rivet_core::object::Signature| -> Vec<u8> {
            let mut line = who.name.to_vec();
            line.extend_from_slice(b" <");
            line.extend_from_slice(&who.email);
            line.push(b'>');
            line
        };
        let any = |patterns: &[super::regex::Regex], lines: &[&[u8]]| {
            patterns
                .iter()
                .any(|p| lines.iter().any(|line| p.is_match(line, false)))
        };
        let author = ident(&commit.author);
        let committer = ident(&commit.committer);
        let body: Vec<&[u8]> = commit.message.split(|b| *b == b'\n').collect();
        let mut passed = true;
        if !self.authors.is_empty() {
            passed &= any(&self.authors, &[&author[..]]);
        }
        if !self.committers.is_empty() {
            passed &= any(&self.committers, &[&committer[..]]);
        }
        if !self.greps.is_empty() {
            let grepped = if self.all_match {
                self.greps
                    .iter()
                    .all(|p| body.iter().any(|line| p.is_match(line, false)))
            } else {
                any(&self.greps, &body)
            };
            passed &= grepped != self.invert;
        } else if self.invert {
            passed = passed && !self.is_empty();
        }
        Ok(passed)
    }
}

/// Git's shortest-unique object name abbreviation over every object in the database.
struct Abbreviator {
    sorted: Vec<String>,
    minimum: usize,
}

impl Abbreviator {
    fn new(repository: &Repository, requested: Option<usize>) -> Result<Self> {
        let mut sorted: Vec<String> = repository
            .odb()
            .oids()?
            .iter()
            .map(|oid| oid.to_string())
            .collect();
        sorted.sort();
        sorted.dedup();
        let hex_len = repository.format().hex_len();
        let minimum = match requested {
            Some(n) if n > 0 => n.clamp(4, hex_len),
            _ => {
                // `find_unique_abbrev`'s default: enough digits for about 2^(bits/2) objects.
                let count = sorted.len().max(1);
                let bits = (usize::BITS - count.leading_zeros()) as usize;
                bits.div_ceil(2).clamp(7, hex_len)
            }
        };
        Ok(Abbreviator { sorted, minimum })
    }

    fn abbreviate(&self, oid: &ObjectId) -> String {
        let hex = oid.to_string();
        let at = self.sorted.binary_search(&hex).unwrap_or_else(|at| at);
        let shared = |other: &String| {
            hex.bytes()
                .zip(other.bytes())
                .take_while(|(a, b)| a == b)
                .count()
        };
        let mut need = self.minimum;
        if at > 0 {
            need = need.max(shared(&self.sorted[at - 1]) + 1);
        }
        let after = if self.sorted.get(at) == Some(&hex) {
            at + 1
        } else {
            at
        };
        if let Some(next) = self.sorted.get(after) {
            need = need.max(shared(next) + 1);
        }
        hex[..need.min(hex.len())].to_string()
    }
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

    /// Sort key: any atom, `-` for descending; the last key given is the primary one.
    #[arg(long = "sort", value_name = "KEY")]
    pub sort: Vec<String>,

    /// Only refs whose tip is reachable from the commit (default `HEAD`).
    #[arg(long = "merged", value_name = "COMMIT", num_args = 0..=1, default_missing_value = "HEAD", allow_hyphen_values = true)]
    pub merged: Vec<String>,

    /// Only refs whose tip is not reachable from the commit (default `HEAD`).
    #[arg(long = "no-merged", value_name = "COMMIT", num_args = 0..=1, default_missing_value = "HEAD", allow_hyphen_values = true)]
    pub no_merged: Vec<String>,

    /// Only refs that contain the commit (default `HEAD`).
    #[arg(long = "contains", value_name = "COMMIT", num_args = 0..=1, default_missing_value = "HEAD", allow_hyphen_values = true)]
    pub contains: Vec<String>,

    /// Only refs that do not contain the commit (default `HEAD`).
    #[arg(long = "no-contains", value_name = "COMMIT", num_args = 0..=1, default_missing_value = "HEAD", allow_hyphen_values = true)]
    pub no_contains: Vec<String>,

    /// Only refs pointing at the object (or at a tag whose target it is).
    #[arg(long = "points-at", value_name = "OBJECT")]
    pub points_at: Vec<String>,

    /// Leave out refs matching this pattern.
    #[arg(long = "exclude", value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Sort case-insensitively.
    #[arg(long = "ignore-case")]
    pub ignore_case: bool,

    /// Respect `%(color:…)` in the format: `always`, `auto` or `never` (default `always` when given).
    #[arg(long = "color", value_name = "WHEN", num_args = 0..=1, require_equals = true, default_missing_value = "always")]
    pub color: Option<String>,

    /// Do not print a line when the format expands to nothing.
    #[arg(long = "omit-empty")]
    pub omit_empty: bool,
}

/// Git's `shorten_unambiguous_ref` for `refname:short`.
pub(super) fn shorten(name: &str, all: &HashSet<&str>) -> String {
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

pub(super) fn ref_pattern_matches(pattern: &str, name: &str) -> bool {
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
    super::foreachref::run(global, args)
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

    /// Find where the commit forked from the history the given ref's reflog recorded.
    #[arg(long = "fork-point")]
    pub fork_point: bool,
}

/// The parents of a commit, empty when the object is not a readable commit.
fn commit_parents(repository: &Repository, oid: ObjectId) -> Vec<ObjectId> {
    let Ok(object) = repository.odb().read(&oid) else {
        return Vec::new();
    };
    if object.kind() != ObjectKind::Commit {
        return Vec::new();
    }
    Commit::parse(repository.format(), object.data())
        .map(|c| c.parents)
        .unwrap_or_default()
}

/// `start` and everything it reaches.
fn commit_ancestors(
    repository: &Repository,
    start: impl IntoIterator<Item = ObjectId>,
) -> HashSet<ObjectId> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<ObjectId> = start.into_iter().collect();
    while let Some(oid) = stack.pop() {
        if seen.insert(oid) {
            stack.extend(commit_parents(repository, oid));
        }
    }
    seen
}

/// Git's `get_fork_point`: the best common ancestor of `commit` and every commit the reflog of
/// `refname` ever recorded, provided there is exactly one and it is one of those recorded commits.
///
/// The recorded commits are the first entry's old value and each entry's new value; a ref with no
/// reflog contributes its current value.
pub(super) fn fork_point(
    repository: &Repository,
    refname: &rivet_core::RefName,
    commit: ObjectId,
) -> Result<Option<ObjectId>> {
    let mut candidates: Vec<ObjectId> = Vec::new();
    let add = |oid: ObjectId, candidates: &mut Vec<ObjectId>| {
        if oid.is_zero() || candidates.contains(&oid) {
            return;
        }
        if let Ok(peeled) = repository.refs().peel_oid(oid, repository.odb())
            && repository
                .odb()
                .read(&peeled)
                .is_ok_and(|o| o.kind() == ObjectKind::Commit)
            && !candidates.contains(&peeled)
        {
            candidates.push(peeled);
        }
    };
    for (i, entry) in repository.refs().reflog(refname)?.iter().enumerate() {
        if i == 0 {
            add(entry.old, &mut candidates);
        }
        add(entry.new, &mut candidates);
    }
    if candidates.is_empty()
        && let Ok(oid) = repository.refs().resolve_oid(refname)
    {
        add(oid, &mut candidates);
    }
    let theirs = commit_ancestors(repository, candidates.iter().copied());
    let ours = commit_ancestors(repository, [commit]);
    let common: HashSet<ObjectId> = ours.intersection(&theirs).copied().collect();
    // The common ancestors are closed under "parent of", so the best ones are those no other
    // common ancestor reaches through a parent.
    let dominated = commit_ancestors(
        repository,
        common
            .iter()
            .flat_map(|oid| commit_parents(repository, *oid)),
    );
    let mut best = common.iter().filter(|oid| !dominated.contains(oid));
    match (best.next(), best.next()) {
        (Some(only), None) if candidates.contains(only) => Ok(Some(*only)),
        _ => Ok(None),
    }
}

/// The reference `name` names, trying the places Git looks in order.
pub(super) fn dwim_ref_name(repository: &Repository, name: &str) -> Option<rivet_core::RefName> {
    let candidates = [
        name.to_string(),
        format!("refs/{name}"),
        format!("refs/tags/{name}"),
        format!("refs/heads/{name}"),
        format!("refs/remotes/{name}"),
        format!("refs/remotes/{name}/HEAD"),
    ];
    candidates
        .iter()
        .filter_map(|c| rivet_core::RefName::from_bytes(c.as_bytes()).ok())
        .find(|r| repository.refs().exists(r))
}

/// What `git merge-base` prints for a usage error, options included.
const MERGE_BASE_USAGE: &str = "usage: git merge-base [-a | --all] <commit> <commit>...
   or: git merge-base [-a | --all] --octopus <commit>...
   or: git merge-base --is-ancestor <commit> <commit>
   or: git merge-base --independent <commit>...
   or: git merge-base --fork-point <ref> [<commit>]

    -a, --[no-]all        output all common ancestors
    --octopus             find ancestors for a single n-way merge
    --independent         list revs not reachable from others
    --is-ancestor         is the first one ancestor of the other?
    --fork-point          find where <commit> forked from reflog of <ref>
";

/// Handler for `rivet merge-base`.
///
/// # Exit codes
///
/// `0` with the base(s) printed; `1` when there is none (or `--is-ancestor` is false); `128` for
/// an unknown commit.
pub fn run_merge_base(global: &GlobalArgs, args: &MergeBaseArgs) -> Result<i32> {
    let repository = super::open(global)?;
    if args.fork_point {
        if args.commits.is_empty() || args.commits.len() > 2 || args.is_ancestor {
            eprintln!("{MERGE_BASE_USAGE}");
            return Ok(129);
        }
        let Some(refname) = dwim_ref_name(&repository, &args.commits[0]) else {
            eprintln!("fatal: No such ref: '{}'", args.commits[0]);
            return Ok(128);
        };
        let spec = args.commits.get(1).map_or("HEAD", String::as_str);
        let Ok(commit) = super::resolve_commit(&repository, spec) else {
            eprintln!("fatal: Not a valid object name: '{spec}'");
            return Ok(128);
        };
        return Ok(match fork_point(&repository, &refname, commit)? {
            Some(oid) => {
                write_stdout(format!("{oid}\n").as_bytes())?;
                0
            }
            None => 1,
        });
    }
    if args.commits.len() < 2 {
        eprintln!("{MERGE_BASE_USAGE}");
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
