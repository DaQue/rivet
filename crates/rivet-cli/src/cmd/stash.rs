//! `stash`: shelve local changes in Git's own layout.
//!
//! A stash is a commit `W` (message `WIP on <branch>: <abbrev> <subject>` or `On <branch>: <msg>`)
//! whose tree is the work tree's tracked state and whose parents are `HEAD` and an index commit
//! `I`. `refs/stash` points at the newest `W` and its reflog is the stack, so `git stash list`
//! reads Rivet's stashes and the reverse. With `-u`/`-a` a third parent `U` (no parents, message
//! `untracked files on <branch>: …`) holds the untracked (and ignored) files. A pathspec limits
//! the work tree changes taken to matching tracked paths. `branch/create/store/save` are not
//! implemented.

use anyhow::Result;
use bstr::{BStr, BString};
use rivet_core::{ObjectId, ObjectKind, RefName, ReflogEntry, Repository};

use super::pretty;
use crate::cli::GlobalArgs;

/// Arguments to `rivet stash`. Without a subcommand the arguments are those of `stash push`.
#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct StashArgs {
    /// The stash operation; `push` when omitted.
    #[command(subcommand)]
    pub command: Option<StashCommand>,

    /// Arguments of the implicit `push`.
    #[command(flatten)]
    pub push: PushArgs,
}

/// The `stash` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum StashCommand {
    /// Save local changes and reset the work tree to `HEAD`.
    Push(PushArgs),
    /// List the stashes, newest first.
    List,
    /// Show the changes a stash records.
    Show(ShowArgs),
    /// Apply a stash and drop it if it applied cleanly.
    Pop(RefArgs),
    /// Apply a stash and keep it.
    Apply(RefArgs),
    /// Remove one stash.
    Drop(RefArgs),
    /// Remove every stash.
    Clear,
}

/// Arguments of `stash push`.
#[derive(Debug, Default, clap::Args)]
pub struct PushArgs {
    /// Use this text as the stash message.
    #[arg(short = 'm', long = "message", value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Also stash untracked files.
    #[arg(short = 'u', long = "include-untracked")]
    pub include_untracked: bool,

    /// Also stash ignored files.
    #[arg(short = 'a', long = "all")]
    pub all: bool,

    /// Leave the index as it is.
    #[arg(short = 'k', long = "keep-index")]
    pub keep_index: bool,

    /// Limit the stash to these paths.
    #[arg(value_name = "PATHSPEC")]
    pub paths: Vec<std::path::PathBuf>,
}

/// A stash named by `stash@{N}` or `N` (default the newest).
#[derive(Debug, clap::Args)]
pub struct RefArgs {
    /// Which stash.
    #[arg(value_name = "STASH")]
    pub stash: Option<String>,

    /// Restore the index too.
    #[arg(long = "index")]
    pub index: bool,
}

/// Arguments of `stash show`.
#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    /// Which stash.
    #[arg(value_name = "STASH")]
    pub stash: Option<String>,

    /// Show the patch.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Show a diffstat (the default).
    #[arg(long = "stat")]
    pub stat: bool,

    /// Show only the names of changed files.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Show names and status letters.
    #[arg(long = "name-status")]
    pub name_status: bool,
}

fn stash_ref() -> RefName {
    RefName::from_bytes(b"refs/stash").expect("valid ref name")
}

/// Handler for `rivet stash`.
///
/// # Exit codes
///
/// * `0` — done (including "No local changes to save").
/// * `1` — no stash entry to operate on, or an apply that conflicted or was refused.
/// * `128` — a fatal error.
pub fn run_stash(global: &GlobalArgs, args: &StashArgs) -> Result<i32> {
    let mut repository = super::open(global)?;
    match &args.command {
        None => {
            if !args.push.paths.is_empty() && !std::env::args().any(|a| a == "--") {
                eprintln!(
                    "error: unknown subcommand: `{}`",
                    args.push.paths[0].display()
                );
                return Ok(129);
            }
            push(global, &mut repository, &args.push)
        }
        Some(StashCommand::Push(push_args)) => push(global, &mut repository, push_args),
        Some(StashCommand::List) => list(&repository),
        Some(StashCommand::Show(show_args)) => show(&repository, show_args),
        Some(StashCommand::Apply(r)) => apply(global, &mut repository, r, false),
        Some(StashCommand::Pop(r)) => apply(global, &mut repository, r, true),
        Some(StashCommand::Drop(r)) => drop_one(&repository, r),
        Some(StashCommand::Clear) => {
            clear(&repository)?;
            Ok(0)
        }
    }
}

fn clear(repository: &Repository) -> Result<()> {
    let name = stash_ref();
    if repository.refs().exists(&name) {
        repository.refs().delete(&name)?;
    }
    rivet_core::refs::reflog::delete(repository.git_dir(), &name)?;
    Ok(())
}

fn matches(specs: &[BString], path: &BString) -> bool {
    specs.is_empty()
        || specs.iter().any(|spec| {
            rivet_core::ops::pathspec_matches(BStr::new(&path[..]), BStr::new(&spec[..]))
        })
}

/// A tree object holding `files` (path to mode and blob).
fn tree_of(
    repository: &Repository,
    files: &std::collections::BTreeMap<BString, (rivet_core::object::EntryMode, ObjectId)>,
) -> Result<ObjectId> {
    use rivet_core::IndexExt as _;
    let mut index = rivet_core::Index::new();
    for (path, (mode, oid)) in files {
        index.insert(rivet_core::ops::make_index_entry(
            BStr::new(&path[..]),
            *mode,
            *oid,
            None,
        ));
    }
    index.sort();
    Ok(index.write_tree(repository.odb())?)
}

fn push(global: &GlobalArgs, repository: &mut Repository, args: &PushArgs) -> Result<i32> {
    let quiet = global.quiet;
    let mut specs = Vec::new();
    for path in &args.paths {
        specs.push(super::worktree_path(repository, path, "stash")?);
    }
    let head = repository.head()?;
    let Some(head_oid) = head.oid() else {
        eprintln!("You do not have the initial commit yet");
        return Ok(1);
    };
    if repository.index().has_conflicts() {
        eprintln!("error: could not write index");
        return Ok(1);
    }
    let head_tree = repository.commit_tree(head_oid)?;
    let index_tree = repository.write_tree()?;
    let full_work_tree = repository.worktree_tree()?;

    if !specs.is_empty() {
        let tracked = rivet_core::ops::flatten_tree(repository.odb(), index_tree)?;
        let known_untracked = args.include_untracked || args.all;
        if !known_untracked
            && let Some(spec) = specs.iter().find(|s| {
                !tracked.keys().any(|p| {
                    rivet_core::ops::pathspec_matches(BStr::new(&p[..]), BStr::new(&s[..]))
                })
            })
        {
            eprintln!("error: pathspec ':(prefix:0){spec}' did not match any file(s) known to git");
            eprintln!("Did you forget to 'git add'?");
            return Ok(128);
        }
    }

    let (work_tree, has_tracked_changes) = if specs.is_empty() {
        (
            full_work_tree,
            index_tree != head_tree || full_work_tree != head_tree,
        )
    } else {
        let odb = repository.odb();
        let head_map = rivet_core::ops::flatten_tree(odb, head_tree)?;
        let index_map = rivet_core::ops::flatten_tree(odb, index_tree)?;
        let work_map = rivet_core::ops::flatten_tree(odb, full_work_tree)?;
        let mut merged = index_map.clone();
        for path in index_map.keys().filter(|p| matches(&specs, p)) {
            match work_map.get(path) {
                Some(entry) => {
                    merged.insert(path.clone(), *entry);
                }
                None => {
                    merged.remove(path);
                }
            }
        }
        let changed = head_map
            .keys()
            .chain(index_map.keys())
            .filter(|p| matches(&specs, p))
            .any(|p| head_map.get(p) != index_map.get(p) || index_map.get(p) != work_map.get(p));
        (tree_of(repository, &merged)?, changed)
    };

    // Untracked (and, with -a, ignored) files, found the way `clean -d` would find them.
    let mut untracked_files: Vec<BString> = Vec::new();
    if args.include_untracked || args.all {
        for (path, dir) in super::clean::untracked_candidates(repository, &args.paths, args.all)? {
            if dir {
                super::clean::files_under(repository, &path, &mut untracked_files)?;
            } else {
                untracked_files.push(path);
            }
        }
        untracked_files.sort();
    }
    if !has_tracked_changes && untracked_files.is_empty() {
        println!("No local changes to save");
        return Ok(0);
    }

    let branch = match head.branch() {
        Some(branch) => branch.short_name().to_string(),
        None => "(no branch)".to_string(),
    };
    let abbreviated = super::history::abbrev(repository, &head_oid, false);
    let subject = {
        let object = repository.odb().read_typed(&head_oid, ObjectKind::Commit)?;
        let commit = rivet_core::Commit::parse(repository.format(), object.data())?;
        String::from_utf8_lossy(&pretty::subject(commit.message)).into_owned()
    };
    let base = format!("{branch}: {abbreviated} {subject}");
    let index_message = format!("index on {base}\n");
    let message = match &args.message {
        Some(text) => format!("On {branch}: {text}"),
        None => format!("WIP on {base}"),
    };

    let index_commit = commit_object(repository, index_tree, &[head_oid], &index_message)?;
    let mut parents = vec![head_oid, index_commit];
    if args.include_untracked || args.all {
        let mut files = std::collections::BTreeMap::new();
        for path in &untracked_files {
            let worktree = repository.worktree()?;
            let Some(stat) = worktree.stat(BStr::new(&path[..]))? else {
                continue;
            };
            let data = worktree.read_file(BStr::new(&path[..]))?;
            let oid = repository.odb().write_raw(ObjectKind::Blob, &data)?;
            files.insert(path.clone(), (stat.mode, oid));
        }
        let tree = tree_of(repository, &files)?;
        let untracked_message = format!("untracked files on {base}\n");
        parents.push(commit_object(repository, tree, &[], &untracked_message)?);
    }
    let stash_commit = commit_object(repository, work_tree, &parents, &message)?;

    let name = stash_ref();
    let old = repository
        .refs()
        .try_read(&name)?
        .map(|_| repository.refs().resolve_oid(&name))
        .transpose()?
        .unwrap_or_else(|| ObjectId::zero(repository.format()));
    repository.refs().set_direct(&name, stash_commit, None)?;
    let who = repository.committer_signature()?;
    let entry = ReflogEntry::new(old, stash_commit, who, BString::from(message.as_str()));
    repository.refs().append_reflog(&name, &entry)?;

    if !untracked_files.is_empty() {
        super::clean::remove_untracked(repository, &args.paths, args.all)?;
    }
    if !specs.is_empty() {
        let tracked = specs_tracked(repository, &specs)?;
        if !tracked.is_empty() {
            repository.restore_paths(Some(head_tree), &tracked, true, true, false)?;
        }
    } else {
        repository.reset_hard_to_tree(head_tree)?;
        // `git stash` resets with `git reset --hard`, which logs to HEAD and records ORIG_HEAD.
        rivet_core::write_orig_head(repository.git_dir(), head_oid)?;
        if head.branch().is_some() {
            let who = repository.committer_signature()?;
            let reset = ReflogEntry::new(
                head_oid,
                head_oid,
                who,
                BString::from("reset: moving to HEAD"),
            );
            repository.refs().append_reflog(&RefName::head(), &reset)?;
        }
        if args.keep_index {
            repository.reset_hard_to_tree(index_tree)?;
        }
    }
    if !quiet {
        println!("Saved working directory and index state {message}");
    }
    Ok(0)
}

/// The tracked paths (index or `HEAD`) the pathspecs select, as exact specs.
fn specs_tracked(repository: &Repository, specs: &[BString]) -> Result<Vec<BString>> {
    let mut out = std::collections::BTreeSet::new();
    for entry in repository.index().entries() {
        if matches(specs, &entry.path) {
            out.insert(entry.path.clone());
        }
    }
    if let Some(tree) = repository.head_tree_oid()? {
        for path in rivet_core::ops::flatten_tree(repository.odb(), tree)?.into_keys() {
            if matches(specs, &path) {
                out.insert(path);
            }
        }
    }
    Ok(out.into_iter().collect())
}

fn commit_object(
    repository: &Repository,
    tree: ObjectId,
    parents: &[ObjectId],
    message: &str,
) -> Result<ObjectId> {
    Ok(repository.write_commit_object(tree, parents, BStr::new(message.as_bytes()))?)
}

/// The stack, newest first.
fn entries(repository: &Repository) -> Result<Vec<ReflogEntry>> {
    let mut all = repository.refs().reflog(&stash_ref())?;
    all.reverse();
    Ok(all)
}

fn list(repository: &Repository) -> Result<i32> {
    use std::io::Write as _;
    let mut out = Vec::new();
    for (position, entry) in entries(repository)?.iter().enumerate() {
        out.extend_from_slice(format!("stash@{{{position}}}: ").as_bytes());
        out.extend_from_slice(&entry.message);
        out.push(b'\n');
    }
    std::io::stdout().write_all(&out)?;
    Ok(0)
}

/// What a stash name resolved to.
enum Picked {
    Found(usize, ObjectId),
    /// Nothing to pick; the message has been printed and this is the exit code.
    Refused,
}

/// Resolve `stash@{N}`, `N` or nothing to (position, stash commit).
fn pick(repository: &Repository, spec: Option<&str>) -> Result<Picked> {
    let stack = entries(repository)?;
    let position = match spec {
        None => 0,
        Some(text) => {
            let inner = text
                .strip_prefix("refs/stash@{")
                .or_else(|| text.strip_prefix("stash@{"))
                .and_then(|rest| rest.strip_suffix('}'))
                .unwrap_or(text);
            inner
                .parse::<usize>()
                .map_err(|_| anyhow::anyhow!("'{text}' is not a stash-like commit"))?
        }
    };
    match stack.get(position) {
        Some(entry) => Ok(Picked::Found(position, entry.new)),
        None if spec.is_some() && !stack.is_empty() => {
            eprintln!("fatal: log for 'stash' only has {} entries", stack.len());
            Ok(Picked::Refused)
        }
        None if spec.is_some() => {
            eprintln!(
                "error: {} is not a valid reference",
                spec.unwrap_or_default()
            );
            Ok(Picked::Refused)
        }
        None => {
            eprintln!("No stash entries found.");
            Ok(Picked::Refused)
        }
    }
}

fn show(repository: &Repository, args: &ShowArgs) -> Result<i32> {
    let Picked::Found(_, stash) = pick(repository, args.stash.as_deref())? else {
        return Ok(128);
    };
    let parents = repository.commit_parents(stash)?;
    let base = repository.commit_tree(parents[0])?;
    let tree = repository.commit_tree(stash)?;
    let options = rivet_core::DiffOptions::new();
    let mut diff =
        rivet_core::tree_diff(repository.format(), base, tree, repository.odb(), &options)?;
    super::detect_renames(repository, &mut diff, &options)?;
    let mut out = Vec::new();
    if args.name_only {
        for file in &diff.files {
            out.extend_from_slice(&rivet_core::ops::quote_path(file.path(), false));
            out.push(b'\n');
        }
    } else if args.name_status {
        rivet_core::format_name_status(&diff, &mut out)?;
    } else if args.patch {
        rivet_core::format_unified(&diff, repository.format(), &mut out)?;
    } else {
        rivet_core::format_stat_sized(&diff, Some(repository.odb()), &mut out)?;
    }
    use std::io::Write as _;
    std::io::stdout().write_all(&out)?;
    Ok(0)
}

fn print_status(global: &GlobalArgs) -> Result<()> {
    let status_args = super::snapshot::StatusArgs {
        paths: Vec::new(),
        porcelain: None,
        short: false,
        untracked: None,
        ignored: false,
        no_refresh: false,
        branch: false,
        ahead_behind: false,
        column: None,
        no_lock_index: false,
    };
    super::snapshot::run_status(global, &status_args)?;
    Ok(())
}

fn apply(
    global: &GlobalArgs,
    repository: &mut Repository,
    args: &RefArgs,
    drop_after: bool,
) -> Result<i32> {
    let Picked::Found(position, stash) = pick(repository, args.stash.as_deref())? else {
        return Ok(128);
    };
    if repository.index().has_conflicts() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in repository.index().entries() {
            if entry.is_unmerged() && seen.insert(entry.path.clone()) {
                println!("{}: needs merge", entry.path);
            }
        }
        if drop_after {
            println!("The stash entry is kept in case you need it again.");
        }
        eprintln!("error: could not write index");
        return Ok(128);
    }
    let Some(head) = repository.head()?.oid() else {
        anyhow::bail!("stash: cannot apply on an unborn branch");
    };
    let parents = repository.commit_parents(stash)?;

    // `--index`: replay the staged part of the stash onto the current index first, remembering
    // the resulting tree to put the index back to after the work tree merge.
    let mut index_target: Option<ObjectId> = None;
    let mut index_patched = false;
    if args.index {
        let base_tree = repository.commit_tree(parents[0])?;
        let stash_index_tree = repository.commit_tree(parents[1])?;
        let current = repository.write_tree()?;
        if base_tree != stash_index_tree && current != stash_index_tree {
            let odb = repository.odb();
            let base_map = rivet_core::ops::flatten_tree(odb, base_tree)?;
            let want_map = rivet_core::ops::flatten_tree(odb, stash_index_tree)?;
            let mut have_map = rivet_core::ops::flatten_tree(odb, current)?;
            let mut clash: Option<BString> = None;
            let paths: std::collections::BTreeSet<&BString> =
                base_map.keys().chain(want_map.keys()).collect();
            for path in paths {
                if base_map.get(path) == want_map.get(path) {
                    continue;
                }
                if have_map.get(path) != base_map.get(path) {
                    clash = Some(path.clone());
                    break;
                }
                match want_map.get(path) {
                    Some(entry) => {
                        have_map.insert(path.clone(), *entry);
                    }
                    None => {
                        have_map.remove(path);
                    }
                }
            }
            if let Some(path) = clash {
                eprintln!("error: {path}: patch does not apply");
                eprintln!("error: conflicts in index. Try without --index.");
                return Ok(128);
            }
            index_target = Some(tree_of(repository, &have_map)?);
            index_patched = true;
        } else {
            index_target = Some(current);
        }
    }

    let index_before = repository.write_tree()?;
    let mut options = rivet_core::MergeOptions::new();
    options.find_renames = super::renames_enabled(repository, "merge.renames");
    options.labels = (
        BString::from("Updated upstream"),
        BString::from("Stashed changes"),
    );
    let nothing_tracked = repository.commit_tree(parents[0])? == repository.commit_tree(stash)?;
    let mut refused: Option<i32> = None;
    let mut conflicted = false;
    if nothing_tracked {
        println!("Already up to date.");
    } else {
        match repository.merge_into_worktree(Some(parents[0]), stash, &options) {
            Ok(result) => {
                conflicted = !super::branching::print_merge_report(&result, "Stashed changes", &[])
                    .is_empty();
            }
            Err(error) => {
                let code = super::branching::report_checkout_refusal(error, "merge")?;
                refused = Some(if code == 1 { 128 } else { code });
            }
        }
    }
    // Untracked files come back after the merge; those whose path is taken are reported and the
    // rest are still restored.
    let untracked_failed = match parents.get(2) {
        Some(untracked) => restore_untracked(repository, *untracked)?,
        None => false,
    };
    if let Some(code) = refused {
        if !global.quiet {
            print_status(global)?;
        }
        if drop_after {
            println!("The stash entry is kept in case you need it again.");
        }
        if untracked_failed {
            eprintln!("error: could not restore untracked files from stash");
        }
        return Ok(code);
    }
    if conflicted {
        if !global.quiet {
            print_status(global)?;
        }
        if drop_after {
            eprintln!("The stash entry is kept in case you need it again.");
        }
        if untracked_failed {
            eprintln!("error: could not restore untracked files from stash");
            return Ok(128);
        }
        return Ok(1);
    }

    if let Some(tree) = index_target {
        repository.reset_index_to_tree(Some(tree))?;
        // After patching the index git runs `git reset`, which leaves a reflog line and ORIG_HEAD.
        if index_patched {
            rivet_core::write_orig_head(repository.git_dir(), head)?;
        }
        if index_patched && repository.head()?.branch().is_some() {
            let who = repository.committer_signature()?;
            let reset = ReflogEntry::new(head, head, who, BString::from("reset: moving to HEAD"));
            repository.refs().append_reflog(&RefName::head(), &reset)?;
        }
        if !global.quiet {
            print_status(global)?;
        }
        if untracked_failed {
            eprintln!("error: could not restore untracked files from stash");
            return Ok(128);
        }
        if drop_after {
            drop_at(repository, position, args.stash.as_deref())?;
        }
        return Ok(0);
    }

    // Changes come back unstaged: the index goes back to what it was, plus the files the stash
    // adds.
    let before_map = rivet_core::ops::flatten_tree(repository.odb(), index_before)?;
    let added: Vec<_> = repository
        .index()
        .entries()
        .iter()
        .filter(|e| !before_map.contains_key(&e.path))
        .cloned()
        .collect();
    repository.reset_index_to_tree(Some(index_before))?;
    for entry in added {
        repository.index_mut().insert(entry);
    }
    repository.index_mut().sort();
    repository.write_index()?;

    if !global.quiet {
        print_status(global)?;
    }
    if untracked_failed {
        eprintln!("error: could not restore untracked files from stash");
        return Ok(128);
    }
    if drop_after {
        drop_at(repository, position, args.stash.as_deref())?;
    }
    Ok(0)
}

/// Restore the files of a stash's untracked commit; returns whether any path was already taken.
fn restore_untracked(repository: &Repository, untracked: ObjectId) -> Result<bool> {
    let tree = repository.commit_tree(untracked)?;
    let files = rivet_core::ops::flatten_tree(repository.odb(), tree)?;
    let mut failed = false;
    for (path, (mode, oid)) in &files {
        let taken = repository.worktree()?.stat(BStr::new(&path[..]))?.is_some();
        if taken {
            eprintln!("{path} already exists, no checkout");
            failed = true;
        } else {
            write_untracked(repository, path, *mode, *oid)?;
        }
    }
    Ok(failed)
}

/// Write one file of the untracked tree of a stash into the work tree.
fn write_untracked(
    repository: &Repository,
    path: &BString,
    mode: rivet_core::object::EntryMode,
    oid: ObjectId,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let worktree = repository.worktree()?;
    let full = worktree.join(BStr::new(&path[..]))?;
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = repository
        .odb()
        .read_typed(&oid, ObjectKind::Blob)?
        .into_data();
    if mode == rivet_core::object::EntryMode::Link {
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&data), &full)?;
    } else {
        std::fs::write(&full, &data)?;
        if mode == rivet_core::object::EntryMode::BlobExecutable {
            std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

fn drop_one(repository: &Repository, args: &RefArgs) -> Result<i32> {
    let Picked::Found(position, _) = pick(repository, args.stash.as_deref())? else {
        return Ok(128);
    };
    drop_at(repository, position, args.stash.as_deref())?;
    Ok(0)
}

/// Remove the stash at `position` (0 = newest), keeping the reflog chain consistent.
fn drop_at(repository: &Repository, position: usize, spec: Option<&str>) -> Result<()> {
    let name = stash_ref();
    let mut stack = repository.refs().reflog(&name)?;
    let index = stack.len() - 1 - position;
    let removed = stack.remove(index);
    let zero = ObjectId::zero(repository.format());
    let mut previous = zero;
    for entry in &mut stack {
        entry.old = previous;
        previous = entry.new;
    }
    let path = name.reflog_path(repository.git_dir());
    if stack.is_empty() {
        clear(repository)?;
    } else {
        let bytes: Vec<u8> = stack.iter().flat_map(ReflogEntry::to_bytes).collect();
        std::fs::write(&path, bytes)
            .map_err(|e| anyhow::anyhow!("could not write '{}': {e}", path.display()))?;
        repository.refs().set_direct(&name, previous, None)?;
    }
    let label = match spec {
        None => format!("refs/stash@{{{position}}}"),
        Some(text) if text.starts_with("stash@{") || text.starts_with("refs/stash@{") => {
            text.to_string()
        }
        Some(_) => format!("refs/stash@{{{position}}}"),
    };
    println!("Dropped {label} ({})", removed.new);
    Ok(())
}
