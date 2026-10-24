//! `worktree`: linked work trees that share one repository.
//!
//! A linked work tree is a directory with a `.git` file (`gitdir: <main>/.git/worktrees/<id>`)
//! and an administrative directory `<main>/.git/worktrees/<id>` holding `gitdir` (the path of
//! that `.git` file), `commondir` (`../..`), `HEAD`, `index`, an optional `locked` file, and the
//! other per-work-tree state. Objects, config and the shared refs stay in the main git directory;
//! [`rivet_core::Repository`] follows `commondir` when it opens such a directory.
//!
//! Implemented: `add`, `list`, `lock`, `unlock`, `remove`, `prune`, `move`, `repair`. Refused by
//! name: `add --orphan`, and `add --track`/`--guess-remote` and `--relative-paths` everywhere.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bstr::{BStr, ByteSlice};
use rivet_core::{HeadState, ObjectId, RefName, Repository};

use crate::cli::GlobalArgs;

/// Arguments of `worktree`: the subcommand and everything after it, parsed in Git's own grammar.
#[derive(Debug, clap::Args)]
pub struct WorktreeArgs {
    /// `add`, `list`, `lock`, `unlock`, `remove`, `prune`, `move` or `repair`, then its arguments.
    #[arg(value_name = "SUBCOMMAND", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE_ALL: &str =
    "usage: git worktree add [-f] [--detach] [--checkout] [--lock [--reason <string>]]
                        [--orphan] [(-b | -B) <new-branch>] <path> [<commit-ish>]
   or: git worktree list [-v | --porcelain [-z]]
   or: git worktree lock [--reason <string>] <worktree>
   or: git worktree move <worktree> <new-path>
   or: git worktree prune [-n] [-v] [--expire <expire>]
   or: git worktree remove [-f] <worktree>
   or: git worktree repair [<path>...]
   or: git worktree unlock <worktree>

";

const USAGE_ADD: &str =
    "usage: git worktree add [-f] [--detach] [--checkout] [--lock [--reason <string>]]
                        [--orphan] [(-b | -B) <new-branch>] <path> [<commit-ish>]

    -f, --[no-]force      checkout <branch> even if already checked out in other worktree
    -b <branch>           create a new branch
    -B <branch>           create or reset a branch
    --[no-]orphan         create unborn branch
    -d, --[no-]detach     detach HEAD at named commit
    --[no-]checkout       populate the new working tree
    --[no-]lock           keep the new working tree locked
    --[no-]reason <string>
                          reason for locking
    -q, --[no-]quiet      suppress progress reporting
    --[no-]track          set up tracking mode (see git-branch(1))
    --[no-]guess-remote   try to match the new branch name with a remote-tracking branch
    --[no-]relative-paths use relative paths for worktrees

";

const USAGE_LOCK: &str = "usage: git worktree lock [--reason <string>] <worktree>

    --[no-]reason <string>
                          reason for locking

";

const USAGE_UNLOCK: &str = "usage: git worktree unlock <worktree>

";

const USAGE_REMOVE: &str = "usage: git worktree remove [-f] <worktree>

    -f, --[no-]force      force removal even if worktree is dirty or locked

";

const USAGE_MOVE: &str = "usage: git worktree move <worktree> <new-path>

    -f, --[no-]force      force move even if worktree is dirty or locked
    --[no-]relative-paths use relative paths for worktrees

";

const USAGE_REPAIR: &str = "usage: git worktree repair [<path>...]

    --[no-]relative-paths use relative paths for worktrees

";

const USAGE_PRUNE: &str = "usage: git worktree prune [-n] [-v] [--expire <expire>]

    -n, --[no-]dry-run    do not remove, show only
    -v, --[no-]verbose    report pruned working trees
    --[no-]expire <expiry-date>
                          prune missing working trees older than <time>

";

/// Where one work tree's `HEAD` points.
pub(super) enum Head {
    /// A branch, by its full ref name.
    Branch(String),
    /// A commit.
    Detached,
}

/// One work tree of the repository, the main one first.
pub(super) struct Entry {
    /// The work tree directory (the git directory itself for a bare repository).
    pub path: PathBuf,
    /// The directory holding its `HEAD`.
    pub admin: PathBuf,
    /// Whether this is the main work tree.
    pub main: bool,
    /// Whether the main repository is bare.
    pub bare: bool,
    /// What `HEAD` names.
    pub head: Head,
    /// The commit `HEAD` resolves to; `None` on an unborn branch.
    pub oid: Option<ObjectId>,
    /// The lock reason (empty when none was given), when the work tree is locked.
    pub locked: Option<String>,
    /// Why `prune` would remove this work tree, if it would.
    pub prunable: Option<&'static str>,
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Parse a `HEAD` file: the branch it names, or the commit it holds.
fn read_head(repository: &Repository, admin: &Path) -> (Head, Option<ObjectId>) {
    let text = read_text(&admin.join("HEAD")).unwrap_or_default();
    let text = text.trim();
    if let Some(target) = text.strip_prefix("ref:") {
        let target = target.trim().to_string();
        let oid = RefName::from_bytes(target.as_bytes())
            .ok()
            .and_then(|name| repository.refs().resolve_oid(&name).ok());
        (Head::Branch(target), oid)
    } else {
        let oid = ObjectId::from_hex(repository.format(), BStr::new(text.as_bytes())).ok();
        (Head::Detached, oid)
    }
}

/// Why `prune` would remove the administrative directory `admin`, if it would.
fn prune_reason(admin: &Path) -> Option<&'static str> {
    let Some(gitdir) = read_text(&admin.join("gitdir")) else {
        return Some("gitdir file does not exist");
    };
    let gitdir = gitdir.trim();
    if gitdir.is_empty() {
        return Some("invalid gitdir file");
    }
    if !Path::new(gitdir).exists() {
        return Some("gitdir file points to non-existent location");
    }
    None
}

/// Every work tree of the repository: the main one, then the linked ones ordered by path.
pub(super) fn entries(repository: &Repository) -> Result<Vec<Entry>> {
    let common = repository.common_dir().to_path_buf();
    let bare = repository
        .config_value(BStr::new(b"core.bare"))?
        .is_some_and(|v| {
            let v = v.to_str_lossy().to_ascii_lowercase();
            v == "true" || v == "yes" || v == "on" || v == "1"
        });
    let main_path = if bare {
        common.clone()
    } else if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().unwrap_or(&common).to_path_buf()
    } else {
        common.clone()
    };
    let (head, oid) = read_head(repository, &common);
    let mut out = vec![Entry {
        path: main_path,
        admin: common.clone(),
        main: true,
        bare,
        head,
        oid,
        locked: None,
        prunable: None,
    }];
    let mut linked: Vec<Entry> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(common.join("worktrees")) {
        for item in dir.flatten() {
            let admin = item.path();
            if !admin.is_dir() {
                continue;
            }
            let Some(gitdir) = read_text(&admin.join("gitdir")) else {
                continue;
            };
            let gitdir = gitdir.trim();
            let path = Path::new(gitdir)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let (head, oid) = read_head(repository, &admin);
            let locked = read_text(&admin.join("locked")).map(|t| t.trim_end().to_string());
            let prunable = if locked.is_some() {
                None
            } else {
                prune_reason(&admin)
            };
            linked.push(Entry {
                path,
                admin,
                main: false,
                bare: false,
                head,
                oid,
                locked,
                prunable,
            });
        }
    }
    linked.sort_by(|a, b| a.path.cmp(&b.path));
    out.extend(linked);
    Ok(out)
}

/// The path of the other work tree that has `branch` (a full ref name) checked out, if any.
pub(super) fn branch_in_use(
    repository: &Repository,
    branch: &str,
    except: Option<&Path>,
) -> Result<Option<PathBuf>> {
    for entry in entries(repository)? {
        if let Head::Branch(name) = &entry.head
            && name == branch
            && except != Some(entry.path.as_path())
        {
            return Ok(Some(entry.path));
        }
    }
    Ok(None)
}

/// Handler for `rivet worktree`.
pub fn run_worktree(global: &GlobalArgs, args: &WorktreeArgs) -> Result<i32> {
    let Some((sub, rest)) = args.args.split_first() else {
        eprintln!("error: need a subcommand");
        eprint!("{USAGE_ALL}");
        return Ok(129);
    };
    match sub.as_str() {
        "add" => worktree_add(global, rest),
        "list" => worktree_list(global, rest),
        "lock" => worktree_lock(global, rest),
        "unlock" => worktree_unlock(global, rest),
        "remove" => worktree_remove(global, rest),
        "prune" => worktree_prune(global, rest),
        "move" => worktree_move(global, rest),
        "repair" => worktree_repair(global, rest),
        other => {
            eprintln!("error: unknown subcommand: `{other}'");
            eprint!("{USAGE_ALL}");
            Ok(129)
        }
    }
}

fn unknown_subcommand(sub: &str, rest: &[String]) -> i32 {
    let mut shown = sub.to_string();
    for word in rest {
        shown.push(' ');
        shown.push_str(word);
    }
    eprintln!("error: unknown subcommand: `{shown}'");
    eprint!("{USAGE_ALL}");
    129
}

fn display_width(path: &Path) -> usize {
    path.to_string_lossy().chars().count()
}

fn worktree_list(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut verbose = false;
    let mut porcelain = false;
    let mut nul = false;
    for arg in args {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--porcelain" => porcelain = true,
            "-z" => nul = true,
            _ => return Ok(unknown_subcommand("list", args)),
        }
    }
    if nul && !porcelain {
        eprintln!("fatal: -z requires --porcelain");
        return Ok(128);
    }
    if verbose && porcelain {
        eprintln!("fatal: options '--verbose' and '--porcelain' cannot be used together");
        return Ok(128);
    }
    let repository = super::open(global)?;
    let list = entries(&repository)?;
    if porcelain {
        let end = if nul { '\0' } else { '\n' };
        let mut text = String::new();
        for entry in &list {
            text.push_str(&format!("worktree {}{end}", entry.path.display()));
            if entry.bare {
                text.push_str(&format!("bare{end}"));
            } else {
                let oid = entry
                    .oid
                    .map(|o| o.to_string())
                    .unwrap_or_else(|| ObjectId::zero(repository.format()).to_string());
                text.push_str(&format!("HEAD {oid}{end}"));
                match &entry.head {
                    Head::Branch(name) => text.push_str(&format!("branch {name}{end}")),
                    Head::Detached => text.push_str(&format!("detached{end}")),
                }
            }
            if let Some(reason) = &entry.locked {
                if reason.is_empty() {
                    text.push_str(&format!("locked{end}"));
                } else {
                    text.push_str(&format!("locked {reason}{end}"));
                }
            }
            if let Some(reason) = entry.prunable {
                text.push_str(&format!("prunable {reason}{end}"));
            }
            text.push(end);
        }
        print!("{text}");
        return Ok(0);
    }
    let width = list
        .iter()
        .map(|e| display_width(&e.path))
        .max()
        .unwrap_or(0);
    for entry in &list {
        let mut line = format!("{:<width$}", entry.path.display());
        if entry.bare {
            line.push_str(" (bare)");
        } else {
            let oid = entry
                .oid
                .unwrap_or_else(|| ObjectId::zero(repository.format()));
            line.push(' ');
            line.push_str(&super::history::abbrev(&repository, &oid, false));
            match &entry.head {
                Head::Branch(name) => {
                    let short = name.strip_prefix("refs/heads/").unwrap_or(name);
                    line.push_str(&format!(" [{short}]"));
                }
                Head::Detached => line.push_str(" (detached HEAD)"),
            }
        }
        let mut notes: Vec<String> = Vec::new();
        if let Some(reason) = &entry.locked {
            if verbose && !reason.is_empty() {
                notes.push(format!("\n\tlocked: {reason}"));
            } else {
                line.push_str(" locked");
            }
        }
        if let Some(reason) = entry.prunable {
            if verbose {
                notes.push(format!("\n\tprunable: {reason}"));
            } else {
                line.push_str(" prunable");
            }
        }
        for note in notes {
            line.push_str(&note);
        }
        println!("{line}");
    }
    Ok(0)
}

/// Find the linked work tree a command-line argument names: by path, or by the trailing
/// components of a path.
fn find_entry<'a>(list: &'a [Entry], spec: &str) -> Option<&'a Entry> {
    let wanted = std::path::absolute(spec)
        .ok()
        .map(|p| normalize(&p))
        .and_then(|p| std::fs::canonicalize(&p).ok().or(Some(p)));
    if let Some(wanted) = &wanted
        && let Some(found) = list.iter().find(|e| {
            std::fs::canonicalize(&e.path).unwrap_or_else(|_| e.path.clone()) == *wanted
                || normalize(&e.path) == *wanted
        })
    {
        return Some(found);
    }
    let suffix = format!("/{}", spec.trim_end_matches('/'));
    let mut hits = list
        .iter()
        .filter(|e| e.path.to_string_lossy().ends_with(&suffix));
    match (hits.next(), hits.next()) {
        (Some(found), None) => Some(found),
        _ => None,
    }
}

/// Collapse `.` and `..` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn worktree_lock(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut reason: Option<String> = None;
    let mut positional: Vec<&String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--reason" => match iter.next() {
                Some(value) => reason = Some(value.clone()),
                None => {
                    eprintln!("error: option `reason' requires a value");
                    eprint!("{USAGE_LOCK}");
                    return Ok(129);
                }
            },
            a if a.starts_with("--reason=") => reason = Some(a["--reason=".len()..].to_string()),
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("error: unknown option `{}'", a.trim_start_matches('-'));
                eprint!("{USAGE_LOCK}");
                return Ok(129);
            }
            _ => positional.push(arg),
        }
    }
    if positional.len() != 1 {
        eprint!("{USAGE_LOCK}");
        return Ok(129);
    }
    let spec = positional[0];
    let repository = super::open(global)?;
    let list = entries(&repository)?;
    let Some(entry) = find_entry(&list, spec) else {
        eprintln!("fatal: '{spec}' is not a working tree");
        return Ok(128);
    };
    if entry.main {
        eprintln!("fatal: The main working tree cannot be locked or unlocked");
        return Ok(128);
    }
    if let Some(existing) = &entry.locked {
        if existing.is_empty() {
            eprintln!("fatal: '{spec}' is already locked");
        } else {
            eprintln!("fatal: '{spec}' is already locked, reason: {existing}");
        }
        return Ok(128);
    }
    let text = match &reason {
        Some(reason) => format!("{reason}\n"),
        None => String::new(),
    };
    std::fs::write(entry.admin.join("locked"), text)?;
    Ok(0)
}

fn worktree_unlock(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut positional: Vec<&String> = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg.len() > 1 {
            eprintln!("error: unknown option `{}'", arg.trim_start_matches('-'));
            eprint!("{USAGE_UNLOCK}");
            return Ok(129);
        }
        positional.push(arg);
    }
    if positional.len() != 1 {
        eprint!("{USAGE_UNLOCK}");
        return Ok(129);
    }
    let spec = positional[0];
    let repository = super::open(global)?;
    let list = entries(&repository)?;
    let Some(entry) = find_entry(&list, spec) else {
        eprintln!("fatal: '{spec}' is not a working tree");
        return Ok(128);
    };
    if entry.main {
        eprintln!("fatal: The main working tree cannot be locked or unlocked");
        return Ok(128);
    }
    if entry.locked.is_none() {
        eprintln!("fatal: '{spec}' is not locked");
        return Ok(128);
    }
    std::fs::remove_file(entry.admin.join("locked"))?;
    Ok(0)
}

/// Remove `<common>/worktrees` when nothing is left in it.
fn remove_worktrees_dir_if_empty(repository: &Repository) {
    let _ = std::fs::remove_dir(repository.common_dir().join("worktrees"));
}

fn worktree_remove(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut force = 0;
    let mut positional: Vec<&String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => force += 1,
            "--no-force" => force = 0,
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("error: unknown option `{}'", a.trim_start_matches('-'));
                eprint!("{USAGE_REMOVE}");
                return Ok(129);
            }
            _ => positional.push(arg),
        }
    }
    if positional.len() != 1 {
        eprint!("{USAGE_REMOVE}");
        return Ok(129);
    }
    let spec = positional[0];
    let repository = super::open(global)?;
    let list = entries(&repository)?;
    let Some(entry) = find_entry(&list, spec) else {
        eprintln!("fatal: '{spec}' is not a working tree");
        return Ok(128);
    };
    if entry.main {
        eprintln!("fatal: '{spec}' is a main working tree");
        return Ok(128);
    }
    if let Some(reason) = &entry.locked
        && force < 2
    {
        if reason.is_empty() {
            eprintln!("fatal: cannot remove a locked working tree;");
        } else {
            eprintln!("fatal: cannot remove a locked working tree, lock reason: {reason}");
        }
        eprintln!("use 'remove -f -f' to override or unlock first");
        return Ok(128);
    }
    if force == 0 && entry.path.exists() {
        let mut inner = Repository::open_with(&entry.admin, None, None)?;
        let status = inner.status(&rivet_core::StatusOptions::new())?;
        if !status.is_clean() {
            eprintln!(
                "fatal: '{spec}' contains modified or untracked files, use --force to delete it"
            );
            return Ok(128);
        }
    }
    if entry.path.exists() {
        std::fs::remove_dir_all(&entry.path)?;
    }
    std::fs::remove_dir_all(&entry.admin)?;
    remove_worktrees_dir_if_empty(&repository);
    Ok(0)
}

/// The text of an I/O error without the `(os error N)` suffix, as `strerror` would print it.
fn errno_text(error: &std::io::Error) -> String {
    let text = error.to_string();
    text.split(" (os error")
        .next()
        .unwrap_or_default()
        .to_string()
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Read a `.git` file. On failure: Git's `READ_GITFILE_ERR_*` code (2 not a file, 5 bad format,
/// 6 no path, 7 not a repository) and the directory the file names, when it names one.
fn read_gitfile(dotgit: &Path) -> std::result::Result<PathBuf, (u8, Option<PathBuf>)> {
    if dotgit.is_dir() {
        return Err((2, None));
    }
    let Some(text) = read_text(dotgit) else {
        return Err((1, None));
    };
    let Some(target) = text.strip_prefix("gitdir: ") else {
        return Err((5, None));
    };
    let target = target.trim_end_matches(['\n', '\r']);
    if target.is_empty() {
        return Err((6, None));
    }
    let mut target = PathBuf::from(target);
    if target.is_relative() {
        target = dotgit.parent().unwrap_or(Path::new("")).join(target);
    }
    if target.is_dir() && target.join("HEAD").exists() {
        Ok(canonical(&target))
    } else {
        Err((7, Some(target)))
    }
}

/// Point `admin/gitdir` and the work tree's `.git` file at each other.
fn write_links(admin: &Path, dotgit: &Path) -> Result<()> {
    std::fs::write(admin.join("gitdir"), format!("{}\n", dotgit.display()))?;
    std::fs::write(dotgit, format!("gitdir: {}\n", admin.display()))?;
    Ok(())
}

fn worktree_move(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut force = 0;
    let mut positional: Vec<&String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-f" | "--force" => force += 1,
            "--no-force" => force = 0,
            "--no-relative-paths" => {}
            "--relative-paths" => {
                anyhow::bail!("worktree move: --relative-paths is not implemented")
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("error: unknown option `{}'", a.trim_start_matches('-'));
                eprint!("{USAGE_MOVE}");
                return Ok(129);
            }
            _ => positional.push(arg),
        }
    }
    if positional.len() != 2 {
        eprint!("{USAGE_MOVE}");
        return Ok(129);
    }
    let (spec, target) = (positional[0], positional[1]);
    let repository = super::open(global)?;
    let list = entries(&repository)?;
    let Some(entry) = find_entry(&list, spec) else {
        eprintln!("fatal: '{spec}' is not a working tree");
        return Ok(128);
    };
    if entry.main {
        eprintln!("fatal: '{spec}' is a main working tree");
        return Ok(128);
    }
    let mut dst = target.clone();
    if Path::new(&dst).is_dir() {
        let Some(name) = entry.path.file_name() else {
            eprintln!(
                "fatal: could not figure out destination name from '{}'",
                entry.path.display()
            );
            return Ok(128);
        };
        dst = format!("{}/{}", dst.trim_end_matches('/'), name.to_string_lossy());
    }
    let dst_path = Path::new(&dst);
    let empty_dir =
        dst_path.is_dir() && std::fs::read_dir(dst_path).is_ok_and(|mut d| d.next().is_none());
    if dst_path.symlink_metadata().is_ok() && !empty_dir {
        eprintln!("fatal: '{dst}' already exists");
        return Ok(128);
    }
    if let Some(reason) = &entry.locked
        && force < 2
    {
        if reason.is_empty() {
            eprintln!("fatal: cannot move a locked working tree;");
        } else {
            eprintln!("fatal: cannot move a locked working tree, lock reason: {reason}");
        }
        eprintln!("use 'move -f -f' to override or unlock first");
        return Ok(128);
    }
    let dotgit = entry.path.join(".git");
    let invalid = if dotgit.symlink_metadata().is_err() {
        Some(format!("'{}' does not exist", dotgit.display()))
    } else {
        match read_gitfile(&dotgit) {
            Err((code, _)) => Some(format!(
                "'{}' is not a .git file, error code {code}",
                dotgit.display()
            )),
            Ok(back) if back != canonical(&entry.admin) => {
                let id = entry
                    .admin
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let at_top = std::env::current_dir()
                    .is_ok_and(|cwd| canonical(&cwd) == canonical(&list[0].path));
                let shown = if at_top {
                    format!(".git/worktrees/{id}")
                } else {
                    entry.admin.display().to_string()
                };
                Some(format!(
                    "'{}' does not point back to '{shown}'",
                    entry.path.display()
                ))
            }
            Ok(_) => None,
        }
    };
    if let Some(why) = invalid {
        eprintln!("fatal: validation failed, cannot move working tree: {why}");
        return Ok(128);
    }
    if let Err(e) = std::fs::rename(&entry.path, &dst) {
        eprintln!(
            "fatal: failed to move '{}' to '{dst}': {}",
            entry.path.display(),
            errno_text(&e)
        );
        return Ok(128);
    }
    write_links(&canonical(&entry.admin), &canonical(dst_path).join(".git"))?;
    Ok(0)
}

fn worktree_repair(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut paths: Vec<&String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--no-relative-paths" => {}
            "--relative-paths" => {
                anyhow::bail!("worktree repair: --relative-paths is not implemented")
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("error: unknown option `{}'", a.trim_start_matches('-'));
                eprint!("{USAGE_REPAIR}");
                return Ok(129);
            }
            _ => paths.push(arg),
        }
    }
    let repository = super::open(global)?;
    let common = canonical(repository.common_dir());
    let mut failed = false;
    for arg in paths {
        failed |= !repair_path(&common, arg)?;
    }
    // Then every registered work tree whose `.git` file does not lead back to its admin directory.
    if let Ok(dir) = std::fs::read_dir(common.join("worktrees")) {
        for item in dir.flatten() {
            let admin = canonical(&item.path());
            let Some(gitdir) = read_text(&admin.join("gitdir")) else {
                continue;
            };
            let Some(path) = Path::new(gitdir.trim()).parent().map(Path::to_path_buf) else {
                continue;
            };
            if !path.exists() {
                continue;
            }
            if !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                failed = true;
                continue;
            }
            let dotgit = canonical(&path).join(".git");
            if dotgit.is_dir() {
                eprintln!("error: .git is not a file: {}", path.display());
                failed = true;
                continue;
            }
            let reason = match read_gitfile(&dotgit) {
                Err(_) => "broken",
                Ok(back) if back != admin => "incorrect",
                Ok(_) => continue,
            };
            eprintln!("repair: .git file {reason}: {}", path.display());
            write_links(&admin, &dotgit)?;
        }
    }
    Ok(i32::from(failed))
}

/// Repair the link between the work tree at `arg` and its administrative directory. `false` when
/// the path cannot be repaired (the diagnostic has been printed).
fn repair_path(common: &Path, arg: &str) -> Result<bool> {
    let Ok(abs) = std::path::absolute(arg) else {
        eprintln!("error: not a valid path: {arg}");
        return Ok(false);
    };
    if !abs.is_dir() {
        eprintln!("error: not a valid path: {arg}");
        return Ok(false);
    }
    let dotgit = canonical(&abs).join(".git");
    let admin = match read_gitfile(&dotgit) {
        Ok(admin) => admin,
        Err((code, target)) => {
            // A moved main repository leaves a stale target; its last component still names the
            // administrative directory of this repository.
            let inferred = target
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| canonical(&common.join("worktrees").join(name)))
                .filter(|dir| dir.is_dir());
            match inferred {
                Some(admin) => admin,
                None => {
                    let why = if code == 7 {
                        "does not reference a repository"
                    } else {
                        "broken"
                    };
                    eprintln!(
                        "error: unable to locate repository; .git file {why}: {}",
                        dotgit.display()
                    );
                    return Ok(false);
                }
            }
        }
    };
    let gitdir_file = admin.join("gitdir");
    let recorded = read_text(&gitdir_file)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let reason = match recorded {
        None => Some("gitdir unreadable"),
        Some(path) if canonical(Path::new(&path)) != dotgit => Some("gitdir incorrect"),
        Some(_) => None,
    };
    if let Some(reason) = reason {
        eprintln!("repair: {reason}: {}", gitdir_file.display());
        write_links(&admin, &dotgit)?;
    }
    Ok(true)
}

fn worktree_prune(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut dry_run = false;
    let mut verbose = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "--expire" => match iter.next().map(String::as_str) {
                Some("now") | Some("all") => {}
                Some(other) => anyhow::bail!("worktree prune: --expire {other} is not implemented"),
                None => {
                    eprintln!("error: option `expire' requires a value");
                    eprint!("{USAGE_PRUNE}");
                    return Ok(129);
                }
            },
            "--expire=now" | "--expire=all" => {}
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("error: unknown option `{}'", a.trim_start_matches('-'));
                eprint!("{USAGE_PRUNE}");
                return Ok(129);
            }
            _ => {
                eprint!("{USAGE_PRUNE}");
                return Ok(129);
            }
        }
    }
    let repository = super::open(global)?;
    let dir = repository.common_dir().join("worktrees");
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let mut names: Vec<_> = read.flatten().map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let admin = dir.join(&name);
        let shown = name.to_string_lossy();
        let reason = if !admin.is_dir() {
            Some("not a valid directory")
        } else if admin.join("locked").exists() {
            None
        } else {
            prune_reason(&admin)
        };
        let Some(reason) = reason else {
            continue;
        };
        if verbose || dry_run {
            eprintln!("Removing worktrees/{shown}: {reason}");
        }
        if !dry_run {
            if admin.is_dir() {
                std::fs::remove_dir_all(&admin)?;
            } else {
                std::fs::remove_file(&admin)?;
            }
        }
    }
    if !dry_run {
        remove_worktrees_dir_if_empty(&repository);
    }
    Ok(0)
}

struct AddOptions {
    force: bool,
    detach: bool,
    checkout: bool,
    lock: bool,
    reason: Option<String>,
    new_branch: Option<String>,
    reset_branch: bool,
    quiet: bool,
}

fn worktree_add(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut options = AddOptions {
        force: false,
        detach: false,
        checkout: true,
        lock: false,
        reason: None,
        new_branch: None,
        reset_branch: false,
        quiet: global.quiet,
    };
    let mut positional: Vec<&String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-f" | "--force" => options.force = true,
            "-d" | "--detach" => options.detach = true,
            "--checkout" => options.checkout = true,
            "--no-checkout" => options.checkout = false,
            "--lock" => options.lock = true,
            "-q" | "--quiet" => options.quiet = true,
            "-b" | "-B" => match iter.next() {
                Some(value) => {
                    options.new_branch = Some(value.clone());
                    options.reset_branch = arg == "-B";
                }
                None => {
                    eprintln!("error: switch `{}' requires a value", &arg[1..]);
                    eprint!("{USAGE_ADD}");
                    return Ok(129);
                }
            },
            "--reason" => match iter.next() {
                Some(value) => options.reason = Some(value.clone()),
                None => {
                    eprintln!("error: option `reason' requires a value");
                    eprint!("{USAGE_ADD}");
                    return Ok(129);
                }
            },
            a if a.starts_with("--reason=") => {
                options.reason = Some(a["--reason=".len()..].to_string())
            }
            "--orphan"
            | "--track"
            | "--no-track"
            | "--guess-remote"
            | "--no-guess-remote"
            | "--relative-paths"
            | "--no-relative-paths" => {
                anyhow::bail!("worktree add: {arg} is not implemented")
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("error: unknown option `{}'", a.trim_start_matches('-'));
                eprint!("{USAGE_ADD}");
                return Ok(129);
            }
            _ => positional.push(arg),
        }
    }
    if positional.is_empty() || positional.len() > 2 {
        eprint!("{USAGE_ADD}");
        return Ok(129);
    }
    if options.reason.is_some() && !options.lock {
        eprintln!("fatal: --reason requires --lock");
        return Ok(128);
    }
    if options.new_branch.is_some() && options.detach {
        eprintln!("fatal: options '-b', '-B', and '--detach' cannot be used together");
        return Ok(128);
    }
    let path_arg = positional[0].as_str();
    let commit_ish = positional.get(1).map(|s| s.as_str());

    let repository = super::open(global)?;

    // What the new work tree starts at, and whether it gets a branch.
    let basename = Path::new(path_arg.trim_end_matches('/'))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut new_branch = options.new_branch.clone();
    let reset_branch = options.reset_branch;
    let mut checkout_branch: Option<String> = None;
    if new_branch.is_none() && !options.detach {
        match commit_ish {
            None => {
                // `git worktree add ../topic` means a branch named after the directory.
                let name = RefName::branch(BStr::new(basename.as_bytes()))?;
                if repository.refs().exists(&name) {
                    checkout_branch = Some(basename.clone());
                } else {
                    new_branch = Some(basename.clone());
                }
            }
            Some(spec) => {
                let as_branch = RefName::branch(BStr::new(spec.as_bytes()));
                if as_branch.is_ok_and(|n| repository.refs().exists(&n)) {
                    checkout_branch = Some(spec.to_string());
                } else {
                    // A commit that is not a branch name: a detached HEAD.
                }
            }
        }
    }
    let start_spec = commit_ish.unwrap_or("HEAD");
    let start: Option<ObjectId> = if let Some(branch) = &checkout_branch {
        let name = RefName::branch(BStr::new(branch.as_bytes()))?;
        Some(repository.refs().resolve_oid(&name)?)
    } else {
        match super::resolve_commit(&repository, start_spec) {
            Ok(oid) => Some(oid),
            Err(_) => {
                // A repository without commits can still add an unborn branch work tree.
                if commit_ish.is_some() || !matches!(repository.head()?, HeadState::Unborn { .. }) {
                    eprintln!("fatal: invalid reference: {start_spec}");
                    return Ok(128);
                }
                None
            }
        }
    };

    if !options.quiet {
        if let Some(branch) = &new_branch {
            let name = RefName::branch(BStr::new(branch.as_bytes()))?;
            if reset_branch && repository.refs().exists(&name) {
                let was = repository.refs().resolve_oid(&name)?;
                eprintln!(
                    "Preparing worktree (resetting branch '{branch}'; was at {})",
                    super::history::abbrev(&repository, &was, false)
                );
            } else {
                eprintln!("Preparing worktree (new branch '{branch}')");
            }
        } else if let Some(branch) = &checkout_branch {
            eprintln!("Preparing worktree (checking out '{branch}')");
        } else if let Some(oid) = start {
            eprintln!(
                "Preparing worktree (detached HEAD {})",
                super::history::abbrev(&repository, &oid, false)
            );
        }
    }

    // Validation: the path, then the branch.
    let base = super::base_dir(global)?;
    let target = normalize(&std::path::absolute(base.join(path_arg))?);
    if target.exists() {
        let empty_dir = target.is_dir()
            && std::fs::read_dir(&target)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
        if !empty_dir {
            eprintln!("fatal: '{path_arg}' already exists");
            return Ok(128);
        }
    }
    let used_branch = checkout_branch.clone().or_else(|| {
        // `-B` onto a branch that is checked out elsewhere is refused the same way.
        new_branch.clone().filter(|_| reset_branch)
    });
    if let Some(branch) = &used_branch
        && !options.force
        && let Some(path) = branch_in_use(&repository, &format!("refs/heads/{branch}"), None)?
    {
        eprintln!(
            "fatal: '{branch}' is already used by worktree at '{}'",
            path.display()
        );
        return Ok(128);
    }
    if let Some(branch) = &new_branch
        && !reset_branch
    {
        let name = RefName::branch(BStr::new(branch.as_bytes()))?;
        if repository.refs().exists(&name) {
            eprintln!("fatal: a branch named '{branch}' already exists");
            return Ok(255);
        }
    }

    // The administrative directory, named after the last path component and made unique.
    let admin_root = repository.common_dir().join("worktrees");
    std::fs::create_dir_all(&admin_root)?;
    let mut id = basename.clone();
    let mut counter = 0;
    while admin_root.join(&id).exists() {
        counter += 1;
        id = format!("{basename}{counter}");
    }
    let admin = admin_root.join(&id);

    // The branch first, as `git branch` would create it.
    if let (Some(branch), Some(oid)) = (&new_branch, start) {
        let name = RefName::branch(BStr::new(branch.as_bytes()))?;
        let who = repository.committer_signature()?;
        let verb = if reset_branch && repository.refs().exists(&name) {
            "Reset to"
        } else {
            "Created from"
        };
        let reason = format!("branch: {verb} {start_spec}");
        repository.set_ref_logged(&name, oid, &who, BStr::new(reason.as_bytes()))?;
    }

    std::fs::create_dir_all(&target)?;
    let target = std::fs::canonicalize(&target)?;
    std::fs::create_dir_all(&admin)?;
    let dot_git = target.join(".git");
    std::fs::write(&dot_git, format!("gitdir: {}\n", admin.display()))?;
    std::fs::write(admin.join("gitdir"), format!("{}\n", dot_git.display()))?;
    std::fs::write(admin.join("commondir"), "../..\n")?;
    let head_text = match (new_branch.as_ref().or(checkout_branch.as_ref()), start) {
        (Some(branch), _) => format!("ref: refs/heads/{branch}\n"),
        (None, Some(oid)) => format!("{oid}\n"),
        (None, None) => "ref: refs/heads/main\n".to_string(),
    };
    std::fs::write(admin.join("HEAD"), head_text)?;
    std::fs::create_dir_all(admin.join("refs"))?;
    std::fs::create_dir_all(admin.join("logs"))?;
    std::fs::write(admin.join("logs/HEAD"), "")?;
    if options.lock {
        let text = match &options.reason {
            Some(reason) => format!("{reason}\n"),
            None => String::new(),
        };
        std::fs::write(admin.join("locked"), text)?;
    }

    if let Some(oid) = start {
        let mut inner = Repository::open_with(&admin, None, None)?;
        // Git creates `HEAD` with `update-ref` (an entry without a message) and then runs
        // `reset --hard`, which logs only when `HEAD` is attached.
        let who = inner.committer_signature()?;
        let created = rivet_core::ReflogEntry::new(
            ObjectId::zero(inner.format()),
            oid,
            who,
            bstr::BString::from(""),
        );
        // Git writes no tab when there is no message; `to_bytes` always writes one.
        let mut line = created.to_bytes();
        line.truncate(line.len() - 2);
        line.push(b'\n');
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(admin.join("logs/HEAD"))?;
        std::io::Write::write_all(&mut log, &line)?;
        if !options.checkout {
            return Ok(0);
        }
        let tree = inner.commit_tree(oid)?;
        inner.reset_hard_to_tree(tree)?;
        inner.move_head(oid, BStr::new(b"reset: moving to HEAD"))?;
        rivet_core::write_orig_head(&admin, oid)?;
        if !options.quiet {
            println!(
                "HEAD is now at {} {}",
                super::short_oid(&oid),
                super::commit_subject(&inner, &oid)?
            );
        }
    }
    Ok(0)
}
