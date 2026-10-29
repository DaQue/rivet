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
/// * **`--all` is refused by name**: Git lists every commit in its in-memory object table order,
///   which is not reproduced here.
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
    if options.all {
        bail!("name-rev: --all is not implemented");
    }

    let repository = super::open(global)?;
    if options.deprecated_stdin {
        eprintln!(
            "warning: --stdin is deprecated. Please use --annotate-stdin instead, which is functionally equivalent.\nThis option will be removed in a future release."
        );
    }
    let names = name_all(&repository, &options)?;

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
