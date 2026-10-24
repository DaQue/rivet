//! `submodule`: `status` and `init`, and `.gitmodules` read-only.
//!
//! Each gitlink of the index prints as `<state><commit> <path>[ (<description>)]`, where the state
//! is `-` for a submodule that is not checked out, `+` when its `HEAD` is not the recorded commit,
//! `U` for a merge conflict and a blank otherwise. Every other subcommand (`update`, `add`,
//! `foreach`, `sync`, `deinit`, `summary`, `absorbgitdirs`, …) is refused by name.

use std::path::Path;

use anyhow::Result;
use bstr::{BStr, BString, ByteSlice};
use rivet_core::{EntryMode, ObjectId, Repository};

use super::query::{DescribeArgs, Described, describe_commit};
use crate::cli::GlobalArgs;

/// Arguments of `submodule`: the subcommand and everything after it.
#[derive(Debug, clap::Args)]
pub struct SubmoduleArgs {
    /// `[--quiet] [--cached] [status [--recursive] [<path>...]]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

fn describe_args(tags: bool, all: bool) -> DescribeArgs {
    DescribeArgs {
        commit: None,
        tags,
        all,
        always: all,
        long: false,
        abbrev: None,
        exact_match: false,
    }
}

/// Handler for `rivet submodule`.
pub fn run_submodule(global: &GlobalArgs, args: &SubmoduleArgs) -> Result<i32> {
    if let Some(at) = args.args.iter().position(|w| !w.starts_with('-')) {
        let quiet = global.quiet || args.args[..at].iter().any(|w| w == "-q" || w == "--quiet");
        let rest = &args.args[at + 1..];
        if args.args[at] == "init" {
            return submodule_init(global, quiet, rest);
        }
    }
    let mut quiet = global.quiet;
    let mut cached = false;
    let mut recursive = false;
    let mut seen_sub = false;
    let mut paths: Vec<String> = Vec::new();
    let mut only_paths = false;
    for word in &args.args {
        if only_paths {
            paths.push(word.clone());
            continue;
        }
        match word.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--cached" => cached = true,
            "--recursive" => recursive = true,
            "--" => only_paths = true,
            "status" if !seen_sub => seen_sub = true,
            other if !seen_sub && !other.starts_with('-') => {
                anyhow::bail!("submodule: '{other}' is not implemented (only 'status' and 'init' are)")
            }
            other if other.starts_with('-') => {
                eprintln!("error: unknown option `{}'", other.trim_start_matches('-'));
                return Ok(129);
            }
            other => paths.push(other.to_string()),
        }
    }
    let repository = super::open(global)?;
    let mut out = String::new();
    let code = status_in(&repository, "", &paths, cached, recursive, &mut out)?;
    if !quiet {
        print!("{out}");
    }
    Ok(code)
}

/// `.gitmodules` as git reads it: from the work tree and, when it is not there, from the index,
/// then from `HEAD`.
fn gitmodules_config(repository: &Repository, root: &Path) -> Option<rivet_core::Config> {
    let from_blob = |oid: &ObjectId| {
        let blob = repository
            .odb()
            .read_typed(oid, rivet_core::ObjectKind::Blob)
            .ok()?;
        rivet_core::Config::parse(blob.data()).ok()
    };
    match std::fs::read(root.join(".gitmodules")) {
        Ok(data) => rivet_core::Config::parse(&data).ok(),
        Err(_) => repository
            .index()
            .entries()
            .iter()
            .find(|e| e.path == ".gitmodules" && !e.is_unmerged())
            .and_then(|e| from_blob(&e.oid))
            .or_else(|| {
                // Last, the file as `HEAD` has it.
                let head = repository.head().ok()?.oid()?;
                let tree = repository.commit_tree(head).ok()?;
                let object = repository.odb().read(&tree).ok()?;
                let parsed = rivet_core::Tree::parse(repository.format(), object.data()).ok()?;
                let entry = parsed.entries.iter().find(|e| e.name == ".gitmodules")?;
                from_blob(&entry.oid)
            }),
    }
}

/// The `path` of every `[submodule "<name>"]` in `.gitmodules`, with the submodule's name.
fn mappings(repository: &Repository, root: &Path) -> Vec<(BString, BString)> {
    let Some(config) = gitmodules_config(repository, root) else {
        return Vec::new();
    };
    config
        .entries()
        .iter()
        .filter(|e| e.section.eq_ignore_ascii_case(b"submodule") && e.key == "path")
        .filter_map(|e| Some((e.value.clone(), e.subsection.clone()?)))
        .collect()
}

fn status_in(
    repository: &Repository,
    prefix: &str,
    paths: &[String],
    cached: bool,
    recursive: bool,
    out: &mut String,
) -> Result<i32> {
    let Some(root) = repository.worktree_path().map(Path::to_path_buf) else {
        return Ok(0);
    };
    let names = mappings(repository, &root);
    let entries = repository.index().entries();

    for spec in paths {
        let spec = BStr::new(spec.as_bytes());
        if !entries
            .iter()
            .any(|e| rivet_core::ops::pathspec_matches(e.path.as_ref(), spec))
        {
            eprintln!("error: pathspec '{spec}' did not match any file(s) known to git");
            return Ok(1);
        }
    }

    let mut done: Vec<&BString> = Vec::new();
    for entry in entries {
        if entry.mode != EntryMode::Commit || done.contains(&&entry.path) {
            continue;
        }
        if !paths.is_empty()
            && !paths.iter().any(|spec| {
                rivet_core::ops::pathspec_matches(entry.path.as_ref(), BStr::new(spec.as_bytes()))
            })
        {
            continue;
        }
        done.push(&entry.path);
        if !names.iter().any(|(path, _)| *path == entry.path) {
            eprintln!(
                "fatal: no submodule mapping found in .gitmodules for path '{}'",
                entry.path
            );
            return Ok(128);
        }
        let shown_path = format!("{prefix}{}", entry.path);
        if entry.is_unmerged() {
            out.push_str(&format!(
                "U{} {shown_path}\n",
                ObjectId::zero(repository.format())
            ));
            continue;
        }
        let sub_dir = root.join(entry.path.to_os_str_lossy());
        let sub = if sub_dir.join(".git").exists() {
            Repository::open_or_discover(&sub_dir).ok()
        } else {
            None
        };
        let head = sub.as_ref().and_then(|s| s.head().ok()?.oid());
        let (Some(sub), Some(head)) = (sub, head) else {
            out.push_str(&format!("-{} {shown_path}\n", entry.oid));
            continue;
        };
        let state = if head == entry.oid { ' ' } else { '+' };
        let shown = if cached { entry.oid } else { head };
        out.push_str(&format!("{state}{shown} {shown_path}"));
        if let Some(text) = describe_for_status(&sub, shown)? {
            out.push_str(&format!(" ({text})"));
        }
        out.push('\n');
        if recursive {
            let nested = format!("{shown_path}/");
            let code = status_in(&sub, &nested, &[], cached, true, out)?;
            if code != 0 {
                return Ok(code);
            }
        }
    }
    Ok(0)
}

/// How `git submodule status` names a commit: `describe`, then `describe --tags`, then
/// `describe --contains`, then `describe --all --always`.
fn describe_for_status(sub: &Repository, oid: ObjectId) -> Result<Option<String>> {
    if !sub.odb().contains(&oid) {
        return Ok(None);
    }
    for args in [describe_args(false, false), describe_args(true, false)] {
        if let Described::Name(name) = describe_commit(sub, oid, &args)? {
            return Ok(Some(name));
        }
    }
    // `--contains` names a commit by a tag that descends from it; that naming is not implemented.
    for (name, tag) in sub.refs().list(b"refs/tags/")? {
        let Ok(tag) = sub.refs().resolve_oid(&name) else {
            let _ = tag;
            continue;
        };
        if let Ok(commit) = sub.refs().peel_oid(tag, sub.odb())
            && commit != oid
            && sub.is_ancestor(oid, commit)?
        {
            anyhow::bail!(
                "submodule status: describing a commit by a later tag is not implemented"
            );
        }
    }
    match describe_commit(sub, oid, &describe_args(false, true))? {
        Described::Name(name) => Ok(Some(name)),
        Described::Failed(_) => Ok(None),
    }
}

const USAGE: &str = "usage: git submodule [--quiet] [--cached]
   or: git submodule [--quiet] add [-b <branch>] [-f|--force] [--name <name>] [--reference <repository>] [--] <repository> [<path>]
   or: git submodule [--quiet] status [--cached] [--recursive] [--] [<path>...]
   or: git submodule [--quiet] init [--] [<path>...]
   or: git submodule [--quiet] deinit [-f|--force] (--all| [--] <path>...)
   or: git submodule [--quiet] update [--init [--filter=<filter-spec>]] [--remote] [-N|--no-fetch] [-f|--force] [--checkout|--merge|--rebase] [--[no-]recommend-shallow] [--reference <repository>] [--recursive] [--[no-]single-branch] [--] [<path>...]
   or: git submodule [--quiet] set-branch (--default|--branch <branch>) [--] <path>
   or: git submodule [--quiet] set-url [--] <path> <newurl>
   or: git submodule [--quiet] summary [--cached|--files] [--summary-limit <n>] [commit] [--] [<path>...]
   or: git submodule [--quiet] foreach [--recursive] <command>
   or: git submodule [--quiet] sync [--recursive] [--] [<path>...]
   or: git submodule [--quiet] absorbgitdirs [--] [<path>...]
";

/// One gitlink of the index.
struct GitLink {
    path: BString,
}

/// The directory the process stands in, relative to the work tree root (`""` at the top).
fn cwd_prefix(root: &Path) -> String {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let Ok(cwd) = std::env::current_dir() else {
        return String::new();
    };
    canonical(&cwd)
        .strip_prefix(canonical(root))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A pathspec typed in `prefix` as a path from the work tree root.
fn from_root(prefix: &str, spec: &str) -> String {
    if prefix.is_empty() || spec.starts_with(':') {
        return spec.to_string();
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in prefix.split('/').chain(spec.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// The gitlinks of the index that the pathspecs select, in index order. `Err(code)` when a
/// pathspec matches nothing at all (the diagnostic has been printed).
fn module_list(
    repository: &Repository,
    prefix: &str,
    paths: &[String],
) -> std::result::Result<Vec<GitLink>, i32> {
    let entries = repository.index().entries();
    let specs: Vec<String> = paths.iter().map(|p| from_root(prefix, p)).collect();
    for (typed, spec) in paths.iter().zip(&specs) {
        let spec = BStr::new(spec.as_bytes());
        if !entries
            .iter()
            .any(|e| rivet_core::ops::pathspec_matches(e.path.as_ref(), spec))
        {
            eprintln!("error: pathspec '{typed}' did not match any file(s) known to git");
            return Err(1);
        }
    }
    let mut out: Vec<GitLink> = Vec::new();
    for entry in entries {
        if entry.mode != EntryMode::Commit || out.iter().any(|l| l.path == entry.path) {
            continue;
        }
        if !specs.is_empty()
            && !specs.iter().any(|spec| {
                rivet_core::ops::pathspec_matches(entry.path.as_ref(), BStr::new(spec.as_bytes()))
            })
        {
            continue;
        }
        out.push(GitLink {
            path: entry.path.clone(),
        });
    }
    Ok(out)
}

/// `path` as seen from `prefix`, the way git prints a submodule path.
fn display_path(prefix: &str, path: &BStr) -> String {
    let shown = super::clean::relative_to(prefix.as_bytes(), path.as_bytes());
    String::from_utf8_lossy(&shown).into_owned()
}

/// The name of the submodule whose `path` is `path`, if `.gitmodules` has one.
fn name_of(gitmodules: &Option<rivet_core::Config>, path: &BStr) -> Option<BString> {
    gitmodules
        .as_ref()?
        .entries()
        .iter()
        .find(|e| {
            e.section.eq_ignore_ascii_case(b"submodule") && e.key == "path" && e.value == path
        })
        .and_then(|e| e.subsection.clone())
}

fn key_of(name: &BStr, key: &str) -> BString {
    let mut out = BString::from("submodule.");
    out.extend_from_slice(name);
    out.push(b'.');
    out.extend_from_slice(key.as_bytes());
    out
}

fn setting(gitmodules: &Option<rivet_core::Config>, name: &BStr, key: &str) -> Option<BString> {
    let key = key_of(name, key);
    gitmodules
        .as_ref()?
        .get(key.as_ref())
        .ok()?
        .map(|v| v.to_owned())
}

/// Whether the submodule is active in this repository: `submodule.<name>.active`, then the
/// `submodule.active` pathspecs, then the presence of `submodule.<name>.url`.
fn is_active(repository: &Repository, name: &BStr, path: &BStr) -> bool {
    let config = repository.config();
    if let Ok(Some(active)) = config.get_bool(key_of(name, "active").as_ref()) {
        return active;
    }
    if let Ok(patterns) = config.get_all(BStr::new("submodule.active"))
        && !patterns.is_empty()
    {
        return patterns
            .iter()
            .any(|p| rivet_core::ops::pathspec_matches(path, p));
    }
    repository
        .config_value(key_of(name, "url").as_ref())
        .ok()
        .flatten()
        .is_some()
}

/// `fatal: invalid value for 'submodule.<name>.update'` for an `update` git does not accept in
/// `.gitmodules` (a `!command` is refused there).
fn check_update_settings(gitmodules: &Option<rivet_core::Config>) -> Option<String> {
    for entry in gitmodules.as_ref()?.entries() {
        if entry.section.eq_ignore_ascii_case(b"submodule")
            && entry.key == "update"
            && !matches!(
                entry.value.to_ascii_lowercase().as_slice(),
                b"checkout" | b"rebase" | b"merge" | b"none"
            )
        {
            return Some(format!(
                "invalid value for 'submodule.{}.update'",
                entry.subsection.clone().unwrap_or_default()
            ));
        }
    }
    None
}

fn url_is_local_not_ssh(url: &str) -> bool {
    match (url.find(':'), url.find('/')) {
        (None, _) => true,
        (Some(colon), Some(slash)) => slash < colon,
        (Some(_), None) => false,
    }
}

/// Remove the last component of `remote`; whether the separator that was cut was a `:`.
fn chop_last_dir(remote: &mut String, is_relative: bool) -> std::result::Result<bool, String> {
    if let Some(at) = remote.rfind('/') {
        remote.truncate(at);
        return Ok(false);
    }
    if let Some(at) = remote.rfind(':') {
        remote.truncate(at);
        return Ok(true);
    }
    if is_relative || remote == "." {
        return Err(format!("cannot strip one component off url '{remote}'"));
    }
    *remote = ".".to_string();
    Ok(false)
}

/// A `./` or `../` url of `.gitmodules` resolved against the superproject's own url (git's
/// `relative_url`).
fn relative_url(remote: &str, url: &str) -> std::result::Result<String, String> {
    let mut remote = remote.to_string();
    if remote.ends_with('/') {
        remote.pop();
    }
    let is_relative = url_is_local_not_ssh(&remote) && !remote.starts_with('/');
    if is_relative && !remote.starts_with("./") && !remote.starts_with("../") {
        remote = format!("./{remote}");
    }
    let mut url = url;
    let mut colon = false;
    loop {
        if let Some(rest) = url.strip_prefix("../") {
            url = rest;
            colon |= chop_last_dir(&mut remote, is_relative)?;
        } else if let Some(rest) = url.strip_prefix("./") {
            url = rest;
        } else {
            break;
        }
    }
    let mut out = format!("{remote}{}{url}", if colon { ":" } else { "/" });
    if url.ends_with('/') {
        out.pop();
    }
    Ok(out.strip_prefix("./").map(str::to_string).unwrap_or(out))
}

/// The url a `./` or `../` entry of `.gitmodules` stands for, from the url of the superproject's
/// remote (or its own location when it has none).
fn resolve_relative_url(repository: &Repository, root: &Path, url: &str) -> Result<String> {
    let branch_remote = repository
        .head()
        .ok()
        .and_then(|head| head.branch().map(|b| b.short_name().to_string()))
        .and_then(|branch| {
            repository
                .config_value(BStr::new(format!("branch.{branch}.remote").as_bytes()))
                .ok()
                .flatten()
        })
        .map(|v| v.to_string())
        .unwrap_or_else(|| "origin".to_string());
    let key = format!("remote.{branch_remote}.url");
    let remote = match repository.config_value(BStr::new(key.as_bytes()))? {
        Some(remote) => remote.to_string(),
        None => {
            eprintln!(
                "warning: could not look up configuration '{key}'. Assuming this repository is its own authoritative upstream."
            );
            root.to_string_lossy().into_owned()
        }
    };
    relative_url(&remote, url).map_err(|e| anyhow::anyhow!(e))
}

fn submodule_init(global: &GlobalArgs, mut quiet: bool, args: &[String]) -> Result<i32> {
    let mut paths: Vec<String> = Vec::new();
    let mut only_paths = false;
    for word in args {
        match word.as_str() {
            _ if only_paths => paths.push(word.clone()),
            "-q" | "--quiet" => quiet = true,
            "--" => only_paths = true,
            other if other.starts_with('-') => {
                eprint!("{USAGE}");
                return Ok(1);
            }
            other => paths.push(other.to_string()),
        }
    }
    let mut repository = super::open(global)?;
    let Some(root) = repository.worktree_path().map(Path::to_path_buf) else {
        return Ok(0);
    };
    let prefix = cwd_prefix(&root);
    let links = match module_list(&repository, &prefix, &paths) {
        Ok(links) => links,
        Err(code) => return Ok(code),
    };
    let gitmodules = gitmodules_config(&repository, &root);
    if let Some(message) = check_update_settings(&gitmodules) {
        eprintln!("fatal: {message}");
        return Ok(128);
    }
    // Without pathspecs, `submodule.active` narrows the list to the active submodules.
    let narrowed = paths.is_empty()
        && repository
            .config()
            .get_all(BStr::new("submodule.active"))
            .is_ok_and(|v| !v.is_empty());
    for link in links {
        if narrowed
            && !name_of(&gitmodules, link.path.as_ref())
                .is_some_and(|name| is_active(&repository, name.as_ref(), link.path.as_ref()))
        {
            continue;
        }
        let shown = display_path(&prefix, link.path.as_ref());
        let name = name_of(&gitmodules, link.path.as_ref());
        let declared = name
            .as_ref()
            .and_then(|name| setting(&gitmodules, name.as_ref(), "url"));
        let (Some(name), Some(declared)) = (name, declared) else {
            eprintln!("fatal: No url found for submodule path '{shown}' in .gitmodules");
            return Ok(128);
        };
        let name_ref: &BStr = name.as_ref();
        if !is_active(&repository, name_ref, link.path.as_ref()) {
            repository
                .config_mut()
                .set(key_of(name_ref, "active").as_ref(), b"true")?;
        }
        let url_key = key_of(name_ref, "url");
        if repository.config_value(url_key.as_ref())?.is_none() {
            let declared = declared.to_string();
            let url = if declared.starts_with("./") || declared.starts_with("../") {
                resolve_relative_url(&repository, &root, &declared)?
            } else {
                declared
            };
            repository
                .config_mut()
                .set(url_key.as_ref(), url.as_bytes())?;
            if !quiet {
                eprintln!("Submodule '{name}' ({url}) registered for path '{shown}'");
            }
        }
        let update_key = key_of(name_ref, "update");
        if repository.config_value(update_key.as_ref())?.is_none()
            && let Some(update) = setting(&gitmodules, name_ref, "update")
        {
            repository
                .config_mut()
                .set(update_key.as_ref(), &update.to_ascii_lowercase())?;
        }
        repository.write_config()?;
    }
    Ok(0)
}
