//! `name-rev`: find symbolic names for revisions, from the refs that reach them.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

use anyhow::{Result, bail};
use bstr::BStr;
use rivet_core::{ObjectId, ObjectKind, Repository};

use super::branching::glob_match;
use super::query::{NameTip, RevName, name_commits, name_tip, refs_sorted, shorten};
use crate::cli::GlobalArgs;

/// Arguments of `name-rev`: everything after the command, in Git's own grammar.
#[derive(Debug, clap::Args)]
pub struct NameRevArgs {
    /// `[--[no-]name-only] [--[no-]tags] [--[no-]refs <pattern>] [--[no-]exclude <pattern>]
    /// [--[no-]annotate-stdin] [--[no-]undefined] [--[no-]always] <commit>...`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE: &str = "usage: git name-rev [<options>] <commit>...
   or: git name-rev [<options>] --all
   or: git name-rev [<options>] --annotate-stdin

    --[no-]name-only      print only ref-based names (no object names)
    --[no-]tags           only use tags to name the commits
    --[no-]refs <pattern> only use refs matching <pattern>
    --[no-]exclude <pattern>
                          ignore refs matching <pattern>

    --[no-]all            list all commits reachable from all refs
    --[no-]annotate-stdin annotate text from stdin
    --[no-]undefined      allow to print `undefined` names (default)
    --[no-]always         show abbreviated commit object as fallback

";

/// Long option names in Git's table order; `--no-` forms are derived from them.
const OPTIONS: [&str; 8] = [
    "name-only",
    "tags",
    "refs",
    "exclude",
    "all",
    "annotate-stdin",
    "undefined",
    "always",
];

#[derive(Default)]
struct Options {
    name_only: bool,
    tags_only: bool,
    refs: Vec<String>,
    exclude: Vec<String>,
    all: bool,
    stdin: bool,
    deprecated_stdin: bool,
    no_undefined: bool,
    always: bool,
}

/// Git's `subpath_matches`: the offset of the first path suffix (starting at a component) that
/// `pattern` matches with no wildmatch flags.
fn subpath_matches(path: &str, pattern: &str) -> Option<usize> {
    let mut offset = 0;
    loop {
        if glob_match(pattern.as_bytes(), &path.as_bytes()[offset..]) {
            return Some(offset);
        }
        offset += path[offset..].find('/')? + 1;
    }
}

/// `rivet name-rev [options] <commit>... | --annotate-stdin`.
///
/// # Caveats
///
/// * **`--all` lists the commits in Git's object table order**, which is the slot order of a
///   hash table filled in the order Git parses things (see [`ObjectTable`]).
/// * Names are computed without Git's commit-date cutoff; the cutoff is an optimisation that does
///   not change a name unless commit dates are badly skewed.
pub fn run_name_rev(global: &GlobalArgs, args: &NameRevArgs) -> Result<i32> {
    let mut options = Options::default();
    let mut positional: Vec<&str> = Vec::new();
    let mut words = args.args.iter();
    while let Some(word) = words.next() {
        if word == "--" {
            positional.extend(words.by_ref().map(String::as_str));
            break;
        }
        if word == "-h" {
            print!("{USAGE}");
            return Ok(0);
        }
        let Some(long) = word.strip_prefix("--") else {
            if word.len() > 1 && word.starts_with('-') {
                eprintln!("error: unknown switch `{}'", &word[1..2]);
                eprint!("{USAGE}");
                return Ok(129);
            }
            positional.push(word);
            continue;
        };
        let (name, inline) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        };
        // Candidates: exact, else unambiguous prefix, over the positive and the `no-` names.
        let candidates: Vec<String> = OPTIONS
            .iter()
            .flat_map(|option| [option.to_string(), format!("no-{option}")])
            .collect();
        let exact = candidates.iter().find(|candidate| *candidate == name);
        let prefixed: Vec<&String> = candidates
            .iter()
            .filter(|candidate| !name.is_empty() && candidate.starts_with(name))
            .collect();
        let chosen = match (exact, prefixed.as_slice()) {
            (Some(exact), _) => exact.as_str(),
            (None, [only]) => only.as_str(),
            _ if name == "stdin" => "stdin",
            (None, []) => {
                eprintln!("error: unknown option `{name}'");
                eprint!("{USAGE}");
                return Ok(129);
            }
            (None, [first, second, ..]) => {
                eprintln!("error: ambiguous option: {name} (could be --{first} or --{second})");
                eprint!("{USAGE}");
                return Ok(129);
            }
        };
        let (negated, base) = match chosen.strip_prefix("no-") {
            Some(base) => (true, base),
            None => (false, chosen),
        };
        let takes_value = !negated && matches!(base, "refs" | "exclude");
        // Git reports these two value errors without the usage block.
        if !takes_value && inline.is_some() {
            eprintln!("error: option `{chosen}' takes no value");
            return Ok(129);
        }
        match base {
            "refs" | "exclude" => {
                let list = if base == "refs" {
                    &mut options.refs
                } else {
                    &mut options.exclude
                };
                if negated {
                    list.clear();
                } else if let Some(value) =
                    inline.map(str::to_string).or_else(|| words.next().cloned())
                {
                    list.push(value);
                } else {
                    eprintln!("error: option `{base}' requires a value");
                    return Ok(129);
                }
            }
            "name-only" => options.name_only = !negated,
            "tags" => options.tags_only = !negated,
            "all" => options.all = !negated,
            "annotate-stdin" => options.stdin = !negated,
            "stdin" => {
                options.stdin = true;
                options.deprecated_stdin = true;
            }
            "undefined" => options.no_undefined = negated,
            "always" => options.always = !negated,
            _ => unreachable!("every option is in the table"),
        }
    }
    if usize::from(options.all) + usize::from(options.stdin) + usize::from(!positional.is_empty())
        > 1
    {
        eprintln!("error: Specify either a list, or --all, not both!");
        eprint!("{USAGE}");
        return Ok(129);
    }
    let repository = super::open(global)?;
    if options.deprecated_stdin {
        eprintln!(
            "warning: --stdin is deprecated. Please use --annotate-stdin instead, which is functionally equivalent.\nThis option will be removed in a future release."
        );
    }
    let names = name_all(&repository, &options)?;

    if options.all {
        let mut out = Vec::new();
        for oid in table_order(&repository, &options)? {
            let prefix = if options.name_only {
                String::new()
            } else {
                format!("{oid} ")
            };
            let line = match names.name_of(&oid)? {
                Some(name) => name,
                None if !options.no_undefined => "undefined".to_string(),
                None if options.always => super::history::abbrev(&repository, &oid, false),
                None => {
                    write_stdout(&out)?;
                    write_stdout(prefix.as_bytes())?;
                    eprintln!("fatal: cannot describe '{oid}'");
                    return Ok(128);
                }
            };
            out.extend_from_slice(format!("{prefix}{line}\n").as_bytes());
        }
        write_stdout(&out)?;
        return Ok(0);
    }

    if options.stdin {
        let mut input = Vec::new();
        std::io::stdin().lock().read_to_end(&mut input)?;
        let mut out = Vec::new();
        let mut lines: Vec<&[u8]> = input.split(|b| *b == b'\n').collect();
        if lines.last().is_some_and(|last| last.is_empty()) {
            lines.pop();
        }
        let last = lines.len().saturating_sub(1);
        let ends_with_newline = input.last() == Some(&b'\n');
        for (number, mut line) in lines.into_iter().enumerate() {
            // Like `strbuf_getline`: a CR goes only together with the LF that ended the line.
            if (number < last || ends_with_newline) && line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            annotate_line(&repository, &names, &options, line, &mut out)?;
            out.push(b'\n');
        }
        write_stdout(&out)?;
        return Ok(0);
    }

    for spec in positional {
        let oid = match super::resolve_rev(&repository, spec) {
            Ok(oid) => oid,
            Err(_) => {
                eprintln!("Could not get sha1 for {spec}. Skipping.");
                continue;
            }
        };
        let target = oid;
        let prefix = if options.name_only {
            String::new()
        } else {
            format!("{spec} ")
        };
        let name = names.name_of(&target)?;
        let line = match name {
            Some(name) => name,
            None if !options.no_undefined => "undefined".to_string(),
            None if options.always => super::history::abbrev(&repository, &target, false),
            None => {
                write_stdout(prefix.as_bytes())?;
                eprintln!("fatal: cannot describe '{target}'");
                return Ok(128);
            }
        };
        write_stdout(format!("{prefix}{line}\n").as_bytes())?;
    }
    Ok(0)
}

/// Git's table of every object it has looked at, reduced to what decides the order `name-rev
/// --all` prints: open addressing over the first four bytes of the id (read little-endian), linear
/// probing, doubled when `nr * 2 >= size - 1`, a rehash in slot order, and a hit found away from its
/// first slot swapped back into that slot.
struct ObjectTable {
    slots: Vec<Option<ObjectId>>,
    count: usize,
}

impl ObjectTable {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            count: 0,
        }
    }

    fn first_slot(oid: &ObjectId, size: usize) -> usize {
        let bytes = oid.as_bytes();
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize & (size - 1)
    }

    fn place(slots: &mut [Option<ObjectId>], oid: ObjectId) {
        let size = slots.len();
        let mut at = Self::first_slot(&oid, size);
        while slots[at].is_some() {
            at = (at + 1) % size;
        }
        slots[at] = Some(oid);
    }

    /// `lookup_object` (swapping a displaced hit forward), else `create_object`.
    fn touch(&mut self, oid: ObjectId) {
        let size = self.slots.len();
        if size > 0 {
            let first = Self::first_slot(&oid, size);
            let mut at = first;
            while let Some(found) = &self.slots[at] {
                if *found == oid {
                    self.slots.swap(at, first);
                    return;
                }
                at = (at + 1) % size;
            }
        }
        if size.saturating_sub(1) <= self.count * 2 {
            let grown = if size < 32 { 32 } else { size * 2 };
            let mut slots = vec![None; grown];
            for old in self.slots.drain(..).flatten() {
                Self::place(&mut slots, old);
            }
            self.slots = slots;
        }
        Self::place(&mut self.slots, oid);
        self.count += 1;
    }
}

/// The commits Git has in its object table after `name-rev` has read every ref and walked the
/// history from the ones that name things, in table order: what `--all` prints.
///
/// The table is filled the way Git fills it: each ref is parsed whether or not a filter keeps it
/// (a commit takes its tree and its parents with it; an annotated tag takes its target, looked up
/// twice), and then the tips, tags first and oldest first, are walked depth first (first parent first),
/// parsing each parent the first time it is met.
fn table_order(repository: &Repository, options: &Options) -> Result<Vec<ObjectId>> {
    struct Facts {
        tree: ObjectId,
        parents: Vec<ObjectId>,
        date: i64,
    }
    let odb = repository.odb();
    let facts_of = |oid: &ObjectId| -> Result<Facts> {
        let object = odb.read_typed(oid, ObjectKind::Commit)?;
        let commit = rivet_core::object::Commit::parse(repository.format(), object.data())?;
        Ok(Facts {
            tree: commit.tree,
            parents: odb.visible_parents(oid, &commit),
            date: commit.committer.when,
        })
    };
    let mut table = ObjectTable::new();
    // Everything the table holds as a commit, parsed or only named as somebody's parent.
    let mut commits: HashSet<ObjectId> = HashSet::new();
    let mut facts: HashMap<ObjectId, Facts> = HashMap::new();
    let mut tips: Vec<(bool, i64, ObjectId)> = Vec::new();
    let parse_commit = |oid: &ObjectId,
                        table: &mut ObjectTable,
                        commits: &mut HashSet<ObjectId>,
                        facts: &mut HashMap<ObjectId, Facts>|
     -> Result<()> {
        if let std::collections::hash_map::Entry::Vacant(entry) = facts.entry(*oid) {
            let parsed = facts_of(oid)?;
            table.touch(parsed.tree);
            for parent in &parsed.parents {
                table.touch(*parent);
                commits.insert(*parent);
            }
            entry.insert(parsed);
        }
        Ok(())
    };
    let tag_target = |oid: &ObjectId| -> Result<ObjectId> {
        let object = odb.read_typed(oid, ObjectKind::Tag)?;
        Ok(rivet_core::object::Tag::parse(repository.format(), object.data())?.object)
    };
    for (refname, oid) in refs_sorted(repository)? {
        let is_tag = refname.starts_with("refs/tags/");
        // `parse_object` on the ref, whatever the filters say: a commit takes its tree and
        // parents, a tag only its target (created, not parsed).
        table.touch(oid);
        let kind = odb.read(&oid)?.kind();
        match kind {
            ObjectKind::Commit => {
                commits.insert(oid);
                parse_commit(&oid, &mut table, &mut commits, &mut facts)?;
            }
            ObjectKind::Tag => {
                let target = tag_target(&oid)?;
                table.touch(target);
                if odb.read(&target)?.kind() == ObjectKind::Commit {
                    commits.insert(target);
                }
            }
            _ => {}
        }
        if !keeps(options, &refname) {
            continue;
        }
        // The tags are peeled one by one, each target parsed in turn.
        let mut current = oid;
        let mut current_kind = kind;
        let mut tagger_date = None;
        while current_kind == ObjectKind::Tag {
            let object = odb.read_typed(&current, ObjectKind::Tag)?;
            let tag = rivet_core::object::Tag::parse(repository.format(), object.data())?;
            tagger_date.get_or_insert(tag.tagger.when);
            table.touch(tag.object);
            current = tag.object;
            current_kind = odb.read(&current)?.kind();
            match current_kind {
                ObjectKind::Commit => {
                    commits.insert(current);
                    parse_commit(&current, &mut table, &mut commits, &mut facts)?;
                }
                ObjectKind::Tag => {
                    let next = tag_target(&current)?;
                    table.touch(next);
                    if odb.read(&next)?.kind() == ObjectKind::Commit {
                        commits.insert(next);
                    }
                }
                _ => {}
            }
        }
        if current_kind != ObjectKind::Commit {
            continue;
        }
        let date = tagger_date.unwrap_or(facts[&current].date);
        tips.push((is_tag, date, current));
    }
    // Tags before everything else, then the older first; equal keys stay in ref order.
    tips.sort_by_key(|(is_tag, date, _)| (!*is_tag, *date));
    let mut named: HashSet<ObjectId> = HashSet::new();
    for (_, _, tip) in tips {
        let mut stack = vec![tip];
        named.insert(tip);
        while let Some(commit) = stack.pop() {
            let parents = facts[&commit].parents.clone();
            let mut fresh = Vec::new();
            for parent in parents {
                parse_commit(&parent, &mut table, &mut commits, &mut facts)?;
                if named.insert(parent) {
                    fresh.push(parent);
                }
            }
            // The first parent is walked first.
            stack.extend(fresh.into_iter().rev());
        }
    }
    Ok(table
        .slots
        .into_iter()
        .flatten()
        .filter(|oid| commits.contains(oid))
        .collect())
}

/// Whether `--tags`, `--exclude` and `--refs` leave `refname` in.
fn keeps(options: &Options, refname: &str) -> bool {
    if options.tags_only && !refname.starts_with("refs/tags/") {
        return false;
    }
    if options
        .exclude
        .iter()
        .any(|pattern| subpath_matches(refname, pattern).is_some())
    {
        return false;
    }
    options.refs.is_empty()
        || options
            .refs
            .iter()
            .any(|pattern| subpath_matches(refname, pattern).is_some())
}

/// What the refs that survive the filters name.
struct Names {
    /// Every reachable commit, by the best name.
    commits: HashMap<ObjectId, RevName>,
    /// Objects that are not commits (annotated tags) and the refs that point straight at them.
    exact: HashMap<ObjectId, Vec<String>>,
}

impl Names {
    /// The name of `oid`: the commit's best name, or the ref that points exactly at a tag object.
    ///
    /// Git picks among several refs on one tag object by where a binary search lands in its tip
    /// table; that choice is refused rather than guessed.
    fn name_of(&self, oid: &ObjectId) -> Result<Option<String>> {
        if let Some(name) = self.commits.get(oid) {
            return Ok(Some(name.text()));
        }
        match self.exact.get(oid).map(Vec::as_slice) {
            None | Some([]) => Ok(None),
            Some([only]) => Ok(Some(only.clone())),
            Some(_) => bail!(
                "name-rev: several refs point at the tag object {oid}; Git's choice among them is not implemented"
            ),
        }
    }
}

/// Name commits from the refs that survive `--tags`, `--refs` and `--exclude`.
fn name_all(repository: &Repository, options: &Options) -> Result<Names> {
    let refs = refs_sorted(repository)?;
    let all_names: HashSet<&str> = refs.iter().map(|(name, _)| name.as_str()).collect();
    let mut tips: Vec<NameTip> = Vec::new();
    let mut exact: HashMap<ObjectId, Vec<String>> = HashMap::new();
    for (refname, oid) in &refs {
        let is_tag = refname.starts_with("refs/tags/");
        if options.tags_only && !is_tag {
            continue;
        }
        if options
            .exclude
            .iter()
            .any(|pattern| subpath_matches(refname, pattern).is_some())
        {
            continue;
        }
        // A pattern that matches from the middle of the ref name lets the output be abbreviated.
        let mut abbreviate = options.tags_only && options.name_only;
        if !options.refs.is_empty() {
            let mut matched = false;
            for pattern in &options.refs {
                if let Some(offset) = subpath_matches(refname, pattern) {
                    matched = true;
                    abbreviate |= offset > 0;
                }
            }
            if !matched {
                continue;
            }
        }
        let short = if abbreviate {
            shorten(refname, &all_names)
        } else {
            refname
                .strip_prefix("refs/heads/")
                .or_else(|| refname.strip_prefix("refs/"))
                .unwrap_or(refname)
                .to_string()
        };
        // A ref at a non-commit (a tag of a tree, say) names that object even though it names no commit.
        if repository.odb().read(oid)?.kind() != ObjectKind::Commit {
            exact.entry(*oid).or_default().push(short.clone());
        }
        if let Some(tip) = name_tip(repository, short, is_tag, *oid)? {
            tips.push(tip);
        }
    }
    Ok(Names {
        commits: name_commits(repository, tips)?,
        exact,
    })
}

/// Copy `line` to `out`, annotating every 40-digit hex id that names a commit with a name.
fn annotate_line(
    repository: &Repository,
    names: &Names,
    options: &Options,
    line: &[u8],
    out: &mut Vec<u8>,
) -> Result<()> {
    let hex_len = repository.format().hex_len();
    let mut index = 0;
    let mut copied = 0;
    while index < line.len() {
        if !line[index].is_ascii_hexdigit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < line.len() && line[index].is_ascii_hexdigit() {
            index += 1;
        }
        if index - start != hex_len {
            continue;
        }
        // Git does not annotate an id spelled in capitals.
        if line[start..index].iter().any(u8::is_ascii_uppercase) {
            continue;
        }
        let Ok(oid) = ObjectId::from_hex(repository.format(), BStr::new(&line[start..index]))
        else {
            continue;
        };
        let Some(name) = names.name_of(&oid)? else {
            continue;
        };
        out.extend_from_slice(&line[copied..start]);
        if options.name_only {
            out.extend_from_slice(name.as_bytes());
        } else {
            out.extend_from_slice(&line[start..index]);
            out.extend_from_slice(format!(" ({name})").as_bytes());
        }
        copied = index;
    }
    out.extend_from_slice(&line[copied..]);
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|_| stdout.flush()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Into::into),
    }
}

#[cfg(test)]
mod table_tests {
    use super::ObjectTable;
    use rivet_core::{ObjectFormat, ObjectId};

    /// An id whose first four bytes, read little-endian, are `low` (so its first slot in a table
    /// of 32 is `low % 32`) and whose remaining bytes tell ids apart.
    fn id(low: u32, tail: u8) -> ObjectId {
        let mut bytes = [tail; 20];
        bytes[..4].copy_from_slice(&low.to_le_bytes());
        ObjectId::from_bytes(ObjectFormat::Sha1, &bytes).unwrap()
    }

    fn order(table: &ObjectTable) -> Vec<ObjectId> {
        table.slots.iter().flatten().copied().collect()
    }

    #[test]
    fn collisions_probe_forward_wrap_and_a_displaced_hit_moves_back() {
        let (a, b, c) = (id(5, 1), id(5, 2), id(31, 3));
        let mut table = ObjectTable::new();
        for oid in [a, b, c] {
            table.touch(oid);
        }
        assert_eq!(table.slots.len(), 32);
        assert_eq!(order(&table), vec![a, b, c]);
        // Looking `b` up again finds it one slot away from where it hashes and swaps it forward.
        table.touch(b);
        assert_eq!(order(&table), vec![b, a, c]);
        // A second id for the last slot wraps around to the front.
        let d = id(63, 4);
        table.touch(d);
        assert_eq!(order(&table), vec![d, b, a, c]);
    }

    #[test]
    fn the_table_doubles_when_half_full_and_rehashes_in_slot_order() {
        let mut table = ObjectTable::new();
        let ids: Vec<ObjectId> = (0..17u32).map(|n| id(n * 3, n as u8)).collect();
        for oid in &ids {
            table.touch(*oid);
        }
        // 16 objects fit in 32 slots; the 17th finds `size - 1 <= 2 * count` and doubles it.
        assert_eq!(table.slots.len(), 64);
        assert_eq!(order(&table), ids);
    }
}
