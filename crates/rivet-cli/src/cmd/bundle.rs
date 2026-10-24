//! `bundle`: `create`, `verify`, `list-heads` and `unbundle` for version-2 bundles, and the check
//! that lets `fetch` read one.
//!
//! The pack written by `create` holds every object undeltified, so a bundle made here is not
//! byte-identical to git's (which deltifies and may be thin); header, object set and behaviour
//! are. `--version=3`, `--filter` and `bundle-uri` capabilities are refused by name.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use bstr::BString;
use rivet_core::bundle::{Bundle, BundleRef, Prerequisite};
use rivet_core::object::{EntryMode, Tag, Tree};
use rivet_core::{ObjectId, ObjectKind, Repository};

use super::gitwalk::{TopoOrder, WalkSpec, sort_topologically, walk};
use crate::cli::GlobalArgs;

/// Arguments of `bundle`: the subcommand and everything after it.
#[derive(Debug, clap::Args)]
pub struct BundleArgs {
    /// `create`, `verify`, `list-heads` or `unbundle`, then its arguments.
    #[arg(value_name = "SUBCOMMAND", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE_ALL: &str = "usage: git bundle create [-q | --quiet | --progress]
                         [--version=<version>] <file> <git-rev-list-args>
   or: git bundle verify [-q | --quiet] <file>
   or: git bundle list-heads <file> [<refname>...]
   or: git bundle unbundle [--progress] <file> [<refname>...]

";

const USAGE_CREATE: &str = "usage: git bundle create [-q | --quiet | --progress]
                         [--version=<version>] <file> <git-rev-list-args>

    -q, --[no-]quiet      do not show progress meter
    --[no-]progress       show progress meter
    --[no-]version <n>    specify bundle format version

";

const USAGE_VERIFY: &str = "usage: git bundle verify [-q | --quiet] <file>

    -q, --[no-]quiet      do not show bundle details

";

const USAGE_LIST: &str = "usage: git bundle list-heads <file> [<refname>...]

";

const USAGE_UNBUNDLE: &str = "usage: git bundle unbundle [--progress] <file> [<refname>...]

    --[no-]progress       show progress meter

";

/// Handler for `rivet bundle`.
pub fn run_bundle(global: &GlobalArgs, args: &BundleArgs) -> Result<i32> {
    let Some((sub, rest)) = args.args.split_first() else {
        eprintln!("error: need a subcommand");
        eprint!("{USAGE_ALL}");
        return Ok(129);
    };
    match sub.as_str() {
        "create" => bundle_create(global, rest),
        "verify" => bundle_verify(global, rest),
        "list-heads" => bundle_list_heads(global, rest),
        "unbundle" => bundle_unbundle(global, rest),
        other => {
            eprintln!("error: unknown subcommand: `{other}'");
            eprint!("{USAGE_ALL}");
            Ok(129)
        }
    }
}

fn need_file(usage: &str) -> i32 {
    eprintln!("fatal: need a <file> argument\n");
    eprint!("{usage}");
    129
}

fn unknown_option(option: &str, usage: &str) -> i32 {
    eprintln!("error: unknown option `{}'", option.trim_start_matches('-'));
    eprint!("{usage}");
    129
}

/// Read the bundle `file`; on failure print git's `error:` line.
fn read_bundle(repository: &Repository, file: &str) -> std::result::Result<Bundle, i32> {
    Bundle::read(Path::new(file), repository.format(), file).map_err(|error| {
        eprintln!("error: {error}");
        1
    })
}

fn print_missing(bundle: &Bundle, repository: &Repository) -> bool {
    let missing = bundle.missing_prerequisites(repository);
    if missing.is_empty() {
        return false;
    }
    eprintln!("error: Repository lacks these prerequisite commits:");
    for p in missing {
        eprintln!("error: {} ", p.oid);
    }
    true
}

fn bundle_list_heads(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut positional: Vec<&String> = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            return Ok(unknown_option(arg, USAGE_LIST));
        }
        positional.push(arg);
    }
    let Some((file, names)) = positional.split_first() else {
        return Ok(need_file(USAGE_LIST));
    };
    let repository = super::open(global)?;
    let bundle = match read_bundle(&repository, file) {
        Ok(bundle) => bundle,
        Err(code) => return Ok(code),
    };
    print_refs(&bundle, names);
    Ok(0)
}

fn print_refs(bundle: &Bundle, names: &[&String]) {
    let mut out = String::new();
    for r in bundle.refs() {
        if names.is_empty() || names.iter().any(|n| n.as_bytes() == r.name.as_slice()) {
            out.push_str(&format!("{} {}\n", r.oid, r.name));
        }
    }
    print!("{out}");
}

fn bundle_verify(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut quiet = global.quiet;
    let mut positional: Vec<&String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            a if a.starts_with('-') && a.len() > 1 => return Ok(unknown_option(a, USAGE_VERIFY)),
            _ => positional.push(arg),
        }
    }
    let Some(file) = positional.first() else {
        return Ok(need_file(USAGE_VERIFY));
    };
    let repository = super::open(global)?;
    let bundle = match read_bundle(&repository, file) {
        Ok(bundle) => bundle,
        Err(code) => return Ok(code),
    };
    if print_missing(&bundle, &repository) {
        return Ok(1);
    }
    let pack = bundle.pack();
    if pack.len() < 12 || !pack.starts_with(b"PACK") {
        eprintln!("error: {file} is not a valid pack");
        return Ok(1);
    }
    eprintln!("{file} is okay");
    let mut out = String::new();
    let plural = |count: usize, one: &str, many: &str| -> String {
        if count == 1 {
            one.to_string()
        } else {
            many.replace("%d", &count.to_string())
        }
    };
    if !quiet {
        let refs = bundle.refs();
        out.push_str(&plural(
            refs.len(),
            "The bundle contains this ref:\n",
            "The bundle contains these %d refs:\n",
        ));
        for r in refs {
            out.push_str(&format!("{} {}\n", r.oid, r.name));
        }
        let prerequisites = bundle.prerequisites();
        if prerequisites.is_empty() {
            out.push_str("The bundle records a complete history.\n");
        } else {
            out.push_str(&plural(
                prerequisites.len(),
                "The bundle requires this ref:\n",
                "The bundle requires these %d refs:\n",
            ));
            for p in prerequisites {
                out.push_str(&format!("{} \n", p.oid));
            }
        }
        out.push_str(&format!(
            "The bundle uses this hash algorithm: {}\n",
            repository.format().as_str()
        ));
    }
    print!("{out}");
    Ok(0)
}

fn bundle_unbundle(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut positional: Vec<&String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--progress" | "--no-progress" => {}
            a if a.starts_with('-') && a.len() > 1 => {
                return Ok(unknown_option(a, USAGE_UNBUNDLE));
            }
            _ => positional.push(arg),
        }
    }
    let Some((file, names)) = positional.split_first() else {
        return Ok(need_file(USAGE_UNBUNDLE));
    };
    let mut repository = super::open(global)?;
    let bundle = match read_bundle(&repository, file) {
        Ok(bundle) => bundle,
        Err(code) => return Ok(code),
    };
    if print_missing(&bundle, &repository) {
        return Ok(1);
    }
    bundle.unpack(&mut repository)?;
    print_refs(&bundle, names);
    Ok(0)
}

/// For `fetch`: when `remote` names a bundle file (directly or through its configured url), check
/// that the repository has the bundle's prerequisites. `Some(code)` when it does not (the
/// diagnostic has been printed).
pub(super) fn check_fetch_source(repository: &Repository, remote: &str) -> Option<i32> {
    let url = match repository.remote_config(remote) {
        Ok(Some(config)) => config.urls.first().map(|u| u.to_string())?,
        _ => remote.to_string(),
    };
    let path = Path::new(url.strip_prefix("file://").unwrap_or(&url));
    if !rivet_core::bundle::looks_like_bundle(path) {
        return None;
    }
    let bundle = Bundle::read(path, repository.format(), &url).ok()?;
    print_missing(&bundle, repository).then_some(1)
}

fn bundle_create(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut quiet = global.quiet;
    let mut version = 2u32;
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        let version_text = match arg.as_str() {
            "--version" => {
                at += 1;
                args.get(at).map(String::as_str)
            }
            a => a.strip_prefix("--version="),
        };
        if let Some(text) = version_text {
            match text.parse::<u32>() {
                Ok(n @ (2 | 3)) => version = n,
                _ => {
                    eprintln!("fatal: unsupported bundle version {text}");
                    return Ok(128);
                }
            }
            at += 1;
            continue;
        }
        match arg.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--progress" | "--no-progress" => {}
            "--" => {
                at += 1;
                break;
            }
            a if a.starts_with('-') && a.len() > 1 => break,
            _ => break,
        }
        at += 1;
    }
    let _ = quiet;
    let Some(file) = args.get(at) else {
        return Ok(need_file(USAGE_CREATE));
    };
    let revs = &args[at + 1..];
    let repository = super::open(global)?;

    // Positive and negative revisions, and the refs the header will carry.
    let mut tips: Vec<(ObjectId, bool)> = Vec::new();
    let mut refs: Vec<BundleRef> = Vec::new();
    let mut pending_tags: Vec<ObjectId> = Vec::new();
    let mut add_ref = |repository: &Repository, name: BString, oid: ObjectId| {
        if !refs.iter().any(|r| r.name == name) {
            refs.push(BundleRef { oid, name });
        }
        let mut current = oid;
        while let Ok(object) = repository.odb().read(&current) {
            if object.kind() != ObjectKind::Tag {
                break;
            }
            if !pending_tags.contains(&current) {
                pending_tags.push(current);
            }
            match Tag::parse(repository.format(), object.data()) {
                Ok(tag) => current = tag.object,
                Err(_) => break,
            }
        }
    };
    let bad_rev = |spec: &str| {
        anyhow::anyhow!(
            "ambiguous argument '{spec}': unknown revision or path not in the working tree.\nUse '--' to separate paths from revisions, like this:\n'git <command> [<revision>...] -- [<file>...]'"
        )
    };
    let peel = |spec: &str| -> Result<ObjectId> {
        super::resolve_commit(&repository, spec).map_err(|_| bad_rev(spec))
    };
    let positive = |spec: &str,
                    tips: &mut Vec<(ObjectId, bool)>,
                    add_ref: &mut dyn FnMut(&Repository, BString, ObjectId)|
     -> Result<()> {
        tips.push((peel(spec)?, false));
        let name = if spec == "HEAD" {
            Some(BString::from("HEAD"))
        } else {
            super::query::dwim_ref_name(&repository, spec).map(|n| BString::from(n.as_bytes()))
        };
        if let Some(name) = name {
            let oid = if name == "HEAD" {
                repository.head()?.oid()
            } else {
                rivet_core::RefName::from_bytes(&name)
                    .ok()
                    .and_then(|n| repository.refs().resolve_oid(&n).ok())
            };
            if let Some(oid) = oid {
                add_ref(&repository, name, oid);
            }
        }
        Ok(())
    };
    let mut flipped = false;
    for spec in revs {
        if spec == "--not" {
            flipped = !flipped;
            continue;
        }
        if flipped && !spec.starts_with('-') {
            let negated = spec.strip_prefix('^').unwrap_or(spec);
            if negated.contains("..") {
                anyhow::bail!("bundle create: ranges after --not are not implemented");
            }
            tips.push((peel(negated)?, true));
            continue;
        }
        if spec == "--all" {
            for (name, oid) in super::query::refs_sorted(&repository)? {
                if let Ok(commit) = repository.refs().peel_oid(oid, repository.odb())
                    && repository
                        .odb()
                        .read(&commit)
                        .is_ok_and(|o| o.kind() == ObjectKind::Commit)
                {
                    tips.push((commit, false));
                }
                add_ref(&repository, BString::from(name), oid);
            }
            if let Some(head) = repository.head()?.oid() {
                tips.push((head, false));
                add_ref(&repository, BString::from("HEAD"), head);
            }
        } else if spec.starts_with('-') && spec.len() > 1 {
            anyhow::bail!("bundle create: {spec} is not implemented");
        } else if let Some(negated) = spec.strip_prefix('^') {
            tips.push((peel(negated)?, true));
        } else if spec.contains("...") {
            anyhow::bail!("bundle create: symmetric ranges are not implemented");
        } else if let Some((left, right)) = spec.split_once("..") {
            let left = if left.is_empty() { "HEAD" } else { left };
            let right = if right.is_empty() { "HEAD" } else { right };
            tips.push((peel(left)?, true));
            positive(right, &mut tips, &mut add_ref)?;
        } else {
            positive(spec, &mut tips, &mut add_ref)?;
        }
    }

    let spec = WalkSpec {
        tips,
        ..Default::default()
    };
    let walked = walk(&repository, &spec)?;
    if refs.is_empty() || walked.commits.is_empty() {
        eprintln!("fatal: Refusing to create empty bundle.");
        return Ok(128);
    }
    let commits = walked.commits.clone();

    // The boundary: parents of listed commits that are not listed, ordered as `--boundary` does.
    let shown: HashSet<ObjectId> = commits.iter().copied().collect();
    let mut candidates: Vec<ObjectId> = Vec::new();
    let mut seen_parent: HashSet<ObjectId> = HashSet::new();
    for oid in &commits {
        for parent in &walked.parents[oid] {
            if !shown.contains(parent) && seen_parent.insert(*parent) {
                candidates.push(*parent);
            }
        }
    }
    let pending: Vec<ObjectId> = candidates.into_iter().rev().collect();
    let boundary = sort_topologically(&repository, pending, TopoOrder::Graph, &walked.parsed)?;
    let mut prerequisites: Vec<Prerequisite> = Vec::new();
    for oid in &boundary {
        prerequisites.push(Prerequisite {
            oid: *oid,
            comment: BString::from(super::rebase::subject_of(&repository, oid)?),
        });
    }

    // The objects: commits, tag objects, then each commit's tree and blobs not already on the
    // boundary.
    let mut objects: Vec<ObjectId> = commits.clone();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    for oid in &boundary {
        let tree = commit_tree(&repository, oid)?;
        mark_tree(&repository, tree, &mut seen)?;
    }
    for tag in &pending_tags {
        if seen.insert(*tag) {
            objects.push(*tag);
        }
    }
    for oid in &commits {
        let tree = commit_tree(&repository, oid)?;
        collect_tree(&repository, tree, &mut seen, &mut objects)?;
    }
    let mut writer = rivet_core::pack::PackWriter::new(repository.format());
    for oid in &objects {
        let object = repository.odb().read(oid)?;
        writer.write_object(&object)?;
    }
    let (pack, _) = writer.finish()?;

    let mut bytes = rivet_core::bundle::header(version, repository.format(), &prerequisites, &refs);
    bytes.extend_from_slice(&pack);
    if file == "-" {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&bytes)?;
        stdout.flush()?;
    } else {
        std::fs::write(file, &bytes).map_err(|e| anyhow::anyhow!("cannot create '{file}': {e}"))?;
    }
    Ok(0)
}

fn commit_tree(repository: &Repository, oid: &ObjectId) -> Result<ObjectId> {
    let object = repository.odb().read_typed(oid, ObjectKind::Commit)?;
    Ok(rivet_core::Commit::parse(repository.format(), object.data())?.tree)
}

fn mark_tree(repository: &Repository, tree: ObjectId, seen: &mut HashSet<ObjectId>) -> Result<()> {
    if !seen.insert(tree) {
        return Ok(());
    }
    let object = repository.odb().read_typed(&tree, ObjectKind::Tree)?;
    let parsed = Tree::parse(repository.format(), object.data())?;
    for entry in &parsed.entries {
        match entry.mode {
            EntryMode::Tree => mark_tree(repository, entry.oid, seen)?,
            EntryMode::Commit => {}
            _ => {
                seen.insert(entry.oid);
            }
        }
    }
    Ok(())
}

fn collect_tree(
    repository: &Repository,
    tree: ObjectId,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    if !seen.insert(tree) {
        return Ok(());
    }
    out.push(tree);
    let object = repository.odb().read_typed(&tree, ObjectKind::Tree)?;
    let parsed = Tree::parse(repository.format(), object.data())?;
    for entry in &parsed.entries {
        match entry.mode {
            EntryMode::Tree => collect_tree(repository, entry.oid, seen, out)?,
            EntryMode::Commit => {}
            _ => {
                if seen.insert(entry.oid) {
                    out.push(entry.oid);
                }
            }
        }
    }
    Ok(())
}
