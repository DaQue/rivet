//! `submodule`: `status`, `init`, `sync`, `add`, `update`, `deinit`, `foreach`, `set-branch`,
//! `set-url`, `absorbgitdirs` and `summary`. `.gitmodules` is read directly; `add`, `set-branch`
//! and `set-url` edit it as text.
//!
//! Each gitlink of the index prints as `<state><commit> <path>[ (<description>)]`, where the state
//! is `-` for a submodule that is not checked out, `+` when its `HEAD` is not the recorded commit,
//! `U` for a merge conflict and a blank otherwise. `summary` lists the commits a gitlink moved
//! across, with `--cached` comparing the index to `HEAD` and `--files` the work tree to the index.

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
        contains: false,
    }
}

/// Handler for `rivet submodule`.
pub fn run_submodule(global: &GlobalArgs, args: &SubmoduleArgs) -> Result<i32> {
    if let Some(at) = args.args.iter().position(|w| !w.starts_with('-')) {
        let quiet = global.quiet || args.args[..at].iter().any(|w| w == "-q" || w == "--quiet");
        let rest = &args.args[at + 1..];
        match args.args[at].as_str() {
            "init" => return submodule_init(global, quiet, rest),
            "sync" => return submodule_sync(global, quiet, rest),
            "add" => return submodule_add(global, quiet, rest),
            "update" => return submodule_update(global, quiet, rest),
            "deinit" => return submodule_deinit(global, quiet, rest),
            "foreach" => return submodule_foreach(global, quiet, rest),
            "set-branch" => return submodule_set_branch(global, quiet, rest),
            "set-url" => return submodule_set_url(global, quiet, rest),
            "absorbgitdirs" => return submodule_absorbgitdirs(global, rest),
            "summary" => return submodule_summary(global, rest),
            _ => {}
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
                eprint!("{USAGE}");
                return Ok(1);
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
        let active = names
            .iter()
            .find(|(path, _)| *path == entry.path)
            .is_some_and(|(path, name)| is_active(repository, name.as_ref(), path.as_ref()));
        let sub = if active && sub_dir.join(".git").exists() {
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
    // A tag that comes after the commit names it (`v1~2`).
    let mut contains = describe_args(true, false);
    contains.contains = true;
    contains.always = false;
    if let Described::Name(name) = describe_commit(sub, oid, &contains)? {
        return Ok(Some(name));
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
pub(super) fn cwd_prefix(root: &Path) -> String {
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
pub(super) fn from_root(prefix: &str, spec: &str) -> String {
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
fn relative_url(
    remote: &str,
    url: &str,
    up_path: Option<&str>,
) -> std::result::Result<String, String> {
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
    let out = out.strip_prefix("./").map(str::to_string).unwrap_or(out);
    match up_path {
        Some(up) if is_relative => Ok(format!("{up}{out}")),
        _ => Ok(out),
    }
}

/// The url a `./` or `../` entry of `.gitmodules` stands for, from the url of the superproject's
/// remote (or its own location when it has none).
fn resolve_relative_url(
    repository: &Repository,
    root: &Path,
    url: &str,
    up_path: Option<&str>,
    quiet: bool,
) -> Result<String> {
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
            if !quiet {
                eprintln!(
                    "warning: could not look up configuration '{key}'. Assuming this repository is its own authoritative upstream."
                );
            }
            root.to_string_lossy().into_owned()
        }
    };
    relative_url(&remote, url, up_path).map_err(|e| anyhow::anyhow!(e))
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
    init_in(&mut repository, &root, &prefix, "", &paths, quiet)
}

/// Register the submodules of `repository` (see [`submodule_init`]); `shown` prefixes the paths
/// in messages when `repository` is itself a submodule.
fn init_in(
    repository: &mut Repository,
    root: &Path,
    prefix: &str,
    shown_prefix: &str,
    paths: &[String],
    quiet: bool,
) -> Result<i32> {
    let links = match module_list(repository, prefix, paths) {
        Ok(links) => links,
        Err(code) => return Ok(code),
    };
    let gitmodules = gitmodules_config(repository, root);
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
                .is_some_and(|name| is_active(repository, name.as_ref(), link.path.as_ref()))
        {
            continue;
        }
        let shown = format!("{shown_prefix}{}", display_path(prefix, link.path.as_ref()));
        let name = name_of(&gitmodules, link.path.as_ref());
        let declared = name
            .as_ref()
            .and_then(|name| setting(&gitmodules, name.as_ref(), "url"));
        let (Some(name), Some(declared)) = (name, declared) else {
            eprintln!("fatal: No url found for submodule path '{shown}' in .gitmodules");
            return Ok(128);
        };
        let name_ref: &BStr = name.as_ref();
        if !is_active(repository, name_ref, link.path.as_ref()) {
            repository
                .config_mut()
                .set(key_of(name_ref, "active").as_ref(), b"true")?;
        }
        let url_key = key_of(name_ref, "url");
        if repository.config_value(url_key.as_ref())?.is_none() {
            let declared = declared.to_string();
            let url = if declared.starts_with("./") || declared.starts_with("../") {
                resolve_relative_url(repository, root, &declared, None, false)?
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

fn canonical(path: &Path) -> std::path::PathBuf {
    // The module directory may not exist yet: resolve its parent and re-append the name.
    match path.file_name() {
        Some(name) if !path.exists() => path
            .parent()
            .map(|parent| canonical(parent).join(name))
            .unwrap_or_else(|| path.to_path_buf()),
        _ => std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    }
}

fn is_relative_url(url: &str) -> bool {
    url.starts_with("./") || url.starts_with("../")
}

/// `../` once per component of `path`, the way back from a submodule to its superproject.
fn up_path(path: &str) -> String {
    "../".repeat(path.trim_end_matches('/').matches('/').count() + 1)
}

fn submodule_sync(global: &GlobalArgs, mut quiet: bool, args: &[String]) -> Result<i32> {
    let mut recursive = false;
    let mut paths: Vec<String> = Vec::new();
    let mut only_paths = false;
    for word in args {
        match word.as_str() {
            _ if only_paths => paths.push(word.clone()),
            "-q" | "--quiet" => quiet = true,
            "--recursive" => recursive = true,
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
    sync_in(
        &mut repository,
        &root,
        &prefix,
        "",
        &paths,
        quiet,
        recursive,
        None,
    )
}

/// A submodule whose name and url the caller has just written to `.gitmodules`; `sync` takes them
/// as they are when the file no longer says (git's `set-url` keeps what it loaded before editing).
struct KnownUrl {
    path: BString,
    name: BString,
    url: String,
}

/// Rewrite the recorded urls of the submodules of `repository`. `prefix` is the cwd inside it
/// (for pathspecs and display), `shown` what the work tree itself is called in messages.
#[allow(clippy::too_many_arguments)]
fn sync_in(
    repository: &mut Repository,
    root: &Path,
    prefix: &str,
    shown: &str,
    paths: &[String],
    quiet: bool,
    recursive: bool,
    known: Option<&KnownUrl>,
) -> Result<i32> {
    let links = match module_list(repository, prefix, paths) {
        Ok(links) => links,
        Err(code) => return Ok(code),
    };
    let gitmodules = gitmodules_config(repository, root);
    if let Some(message) = check_update_settings(&gitmodules) {
        eprintln!("fatal: {message}");
        return Ok(128);
    }
    for link in links {
        let known = known.filter(|k| k.path == link.path);
        let Some(name) =
            name_of(&gitmodules, link.path.as_ref()).or_else(|| known.map(|k| k.name.clone()))
        else {
            continue;
        };
        if !is_active(repository, name.as_ref(), link.path.as_ref()) {
            continue;
        }
        let display = format!("{shown}{}", display_path(prefix, link.path.as_ref()));
        let Some(declared) = setting(&gitmodules, name.as_ref(), "url")
            .or_else(|| known.map(|k| BString::from(k.url.as_str())))
        else {
            eprintln!("fatal: No url found for submodule path '{display}' in .gitmodules");
            return Ok(128);
        };
        let declared = declared.to_string();
        let path_text = link.path.to_string();
        let (url, origin_url) = if is_relative_url(&declared) {
            (
                resolve_relative_url(repository, root, &declared, None, true)?,
                resolve_relative_url(
                    repository,
                    root,
                    &declared,
                    Some(&up_path(&path_text)),
                    true,
                )?,
            )
        } else {
            (declared.clone(), declared)
        };
        if !quiet {
            println!("Synchronizing submodule url for '{display}'");
        }
        repository
            .config_mut()
            .set(key_of(name.as_ref(), "url").as_ref(), url.as_bytes())?;
        repository.write_config()?;
        let sub_dir = root.join(link.path.to_os_str_lossy());
        if !sub_dir.join(".git").exists() {
            continue;
        }
        let Ok(mut sub) = Repository::open_or_discover(&sub_dir) else {
            continue;
        };
        let remote = sub
            .head()
            .ok()
            .and_then(|head| head.branch().map(|b| b.short_name().to_string()))
            .and_then(|branch| {
                sub.config_value(BStr::new(format!("branch.{branch}.remote").as_bytes()))
                    .ok()
                    .flatten()
            })
            .map(|v| v.to_string())
            .unwrap_or_else(|| "origin".to_string());
        sub.config_mut().set(
            BStr::new(format!("remote.{remote}.url").as_bytes()),
            origin_url.as_bytes(),
        )?;
        sub.write_config()?;
        if recursive {
            let nested = format!("{display}/");
            let sub_root = sub
                .worktree_path()
                .map(Path::to_path_buf)
                .unwrap_or(sub_dir);
            let code = sync_in(&mut sub, &sub_root, "", &nested, &[], quiet, true, None)?;
            if code != 0 {
                return Ok(code);
            }
        }
    }
    Ok(0)
}

const USAGE_ADD: &str = "usage: git submodule add [<options>] [--] <repository> [<path>]

    -b, --[no-]branch <branch>
                          branch of repository to add as submodule
    -f, --[no-]force      allow adding an otherwise ignored submodule path
    -q, --[no-]quiet      print only error messages
    --[no-]progress       force cloning progress
    --[no-]reference <repository>
                          reference repository
    --[no-]ref-format <format>
                          specify the reference format to use
    --[no-]dissociate     borrow the objects from reference repositories
    --[no-]name <name>    sets the submodule's name to the given string instead of defaulting to its path
    --[no-]depth <n>      depth for shallow clones

";

/// `to` as a path from the directory `from` (both absolute).
fn relative_path(from: &Path, to: &Path) -> std::path::PathBuf {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = std::path::PathBuf::new();
    for _ in shared..from.len() {
        out.push("..");
    }
    for part in &to[shared..] {
        out.push(part.as_os_str());
    }
    out
}

/// The last component of a repository url, without `.git`: the default submodule path.
fn url_basename(repo: &str) -> String {
    let trimmed = repo.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_string()
}

/// A path as a list of components with `.` and `..` resolved; `None` when it leaves the tree.
pub(super) fn normalize_inside(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn submodule_add(global: &GlobalArgs, mut quiet: bool, args: &[String]) -> Result<i32> {
    let mut force = false;
    let mut branch: Option<String> = None;
    let mut name: Option<String> = None;
    let mut positional: Vec<&String> = Vec::new();
    let mut iter = args.iter();
    while let Some(word) = iter.next() {
        if !positional.is_empty() {
            if word.starts_with('-') {
                eprint!("{USAGE_ADD}");
                return Ok(129);
            }
            positional.push(word);
            continue;
        }
        let mut value = |what: &mut Option<String>| match iter.next() {
            Some(v) if !v.is_empty() => {
                *what = Some(v.clone());
                true
            }
            _ => false,
        };
        let ok = match word.as_str() {
            "-q" | "--quiet" => {
                quiet = true;
                true
            }
            "-f" | "--force" => {
                force = true;
                true
            }
            "-b" | "--branch" => value(&mut branch),
            "--name" => value(&mut name),
            w if w.starts_with("--branch=") => {
                branch = Some(w["--branch=".len()..].to_string());
                true
            }
            w if w.starts_with("--name=") => {
                name = Some(w["--name=".len()..].to_string());
                true
            }
            "--depth" | "--reference" | "--ref-format" | "--progress" | "--dissociate" => {
                anyhow::bail!("submodule add: {word} is not implemented")
            }
            w if w.starts_with("--depth=")
                || w.starts_with("--reference=")
                || w.starts_with("--ref-format=") =>
            {
                anyhow::bail!("submodule add: {word} is not implemented")
            }
            "--" => {
                positional.extend(iter.by_ref());
                break;
            }
            w if w.starts_with('-') => false,
            _ => {
                positional.push(word);
                true
            }
        };
        if !ok {
            eprint!("{USAGE}");
            return Ok(1);
        }
    }
    if positional.is_empty() || positional.len() > 2 {
        if positional.is_empty() {
            eprint!("{USAGE}");
            return Ok(1);
        }
        eprint!("{USAGE_ADD}");
        return Ok(129);
    }
    let repo = positional[0].clone();
    let typed_path = match positional.get(1) {
        Some(path) => (*path).clone(),
        None => url_basename(&repo),
    };

    let mut repository = super::open(global)?;
    let Some(root) = repository.worktree_path().map(Path::to_path_buf) else {
        anyhow::bail!("submodule add: this operation must be run in a work tree");
    };
    let prefix = cwd_prefix(&root);
    let joined = from_root(&prefix, typed_path.trim_end_matches('/'));
    if typed_path.is_empty() || joined == "." {
        eprintln!(
            "fatal: empty string is not a valid pathspec. please use . instead if you meant to match all paths"
        );
        return Ok(128);
    }
    let Some(sm_path) = normalize_inside(&joined) else {
        eprintln!(
            "fatal: {typed_path}: '{typed_path}' is outside repository at '{}'",
            root.display()
        );
        return Ok(128);
    };
    let sm_name = name.clone().unwrap_or_else(|| sm_path.clone());

    // The url the superproject records in its config.
    let realrepo = if is_relative_url(&repo) {
        if !prefix.is_empty() {
            eprintln!(
                "fatal: Relative path can only be used from the toplevel of the working tree"
            );
            return Ok(128);
        }
        resolve_relative_url(&repository, &root, &repo, None, true)?
    } else if repo.starts_with('/') || !url_is_local_not_ssh(&repo) {
        repo.clone()
    } else {
        eprintln!("fatal: repo URL: '{repo}' must be absolute or begin with ./|../");
        return Ok(128);
    };

    let dest = root.join(&sm_path);
    let existing = repository
        .index()
        .entries()
        .iter()
        .any(|e| e.path == sm_path.as_bytes());
    if !force && !existing && is_ignored(&repository, &sm_path, true)? {
        let mut hints = format!(
            "The following paths are ignored by one of your .gitignore files:\n{sm_path}\n"
        );
        let advice = repository
            .config_value(BStr::new("advice.addIgnoredFile"))?
            .and_then(|v| rivet_core::parse_bool(v.as_ref()).ok())
            .unwrap_or(true);
        if advice {
            hints.push_str("hint: Use -f if you really want to add them.\n");
            hints.push_str(
                "hint: Disable this message with \"git config set advice.addIgnoredFile false\"\n",
            );
        }
        eprint!("{hints}");
        return Ok(1);
    }
    if existing {
        eprintln!("fatal: '{sm_path}' already exists in the index");
        return Ok(128);
    }
    let populated = dest.join(".git").exists();
    if dest.symlink_metadata().is_ok() && !populated {
        eprintln!("fatal: '{sm_path}' already exists and is not a valid git repo");
        return Ok(128);
    }
    let git_dir = repository.common_dir().join("modules").join(&sm_name);
    if git_dir.exists() && !populated {
        if !force {
            let remotes = rivet_core::Config::load(&git_dir.join("config"))
                .map(|c| {
                    c.entries()
                        .iter()
                        .filter(|e| e.section.eq_ignore_ascii_case(b"remote") && e.key == "url")
                        .map(|e| {
                            format!(
                                "  {}\t{}\n",
                                e.subsection.clone().unwrap_or_default(),
                                e.value
                            )
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            eprintln!(
                "fatal: A git directory for '{sm_name}' is found locally with remote(s):\n{remotes}If you want to reuse this local git directory instead of cloning again from\n  {realrepo}\nuse the '--force' option. If the local git directory is not the correct repo\nor you are unsure what this means choose another name with the '--name' option."
            );
            return Ok(128);
        }
        anyhow::bail!(
            "submodule add: reusing a local git directory with --force is not implemented"
        );
    }

    if populated {
        if !quiet {
            println!("Adding existing repo at '{sm_path}' to the index");
        }
    } else {
        if let Err(code) = clone_submodule(&dest, &git_dir, &realrepo, quiet, None, None) {
            return Ok(code);
        }
        if let Err(code) = checkout_branch(&dest, &git_dir, branch.as_deref(), &sm_path) {
            return Ok(code);
        }
    }

    // Stage the gitlink at the submodule's `HEAD`.
    let sub = Repository::open_with(&dest.join(".git"), Some(&dest), None)
        .or_else(|_| Repository::open_with(&git_dir, Some(&dest), None))?;
    let Some(head) = sub.head()?.oid() else {
        eprintln!("fatal: '{sm_path}' does not have a commit checked out");
        return Ok(128);
    };
    stage_entry(&mut repository, &sm_path, head, EntryMode::Commit)?;

    // `.gitmodules`, then its staged copy.
    let gitmodules_path = root.join(".gitmodules");
    let mut entries = vec![("path", sm_path.as_str()), ("url", repo.as_str())];
    if let Some(branch) = &branch {
        entries.push(("branch", branch.as_str()));
    }
    let mut text = std::fs::read_to_string(&gitmodules_path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "[submodule \"{}\"]\n",
        sm_name.replace('\\', "\\\\").replace('"', "\\\"")
    ));
    for (key, value) in entries {
        text.push_str(&format!("\t{key} = {}\n", config_value(value)));
    }
    std::fs::write(&gitmodules_path, text)?;
    let bytes = std::fs::read(&gitmodules_path)?;
    let blob = repository
        .odb()
        .write_raw(rivet_core::ObjectKind::Blob, &bytes)?;
    stage_entry(&mut repository, ".gitmodules", blob, EntryMode::Blob)?;

    // Finally the superproject's own config.
    let name_ref: &BStr = sm_name.as_bytes().as_bstr();
    repository
        .config_mut()
        .set(key_of(name_ref, "url").as_ref(), realrepo.as_bytes())?;
    repository
        .config_mut()
        .set(key_of(name_ref, "active").as_ref(), b"true")?;
    repository.write_config()?;
    Ok(0)
}

fn stage_entry(
    repository: &mut Repository,
    path: &str,
    oid: ObjectId,
    mode: EntryMode,
) -> Result<()> {
    use rivet_core::{IndexEntry, IndexEntryExtendedFlags, IndexEntryFlags, Stage};
    let entry = IndexEntry {
        ctime_seconds: 0,
        ctime_nanos: 0,
        mtime_seconds: 0,
        mtime_nanos: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: IndexEntryFlags::with(Stage::Normal, path.len()),
        extended_flags: IndexEntryExtendedFlags::empty().bits(),
        path: BString::from(path),
    };
    repository.index_mut().insert(entry);
    repository.index_mut().sort();
    repository.write_index()?;
    Ok(())
}

/// Whether `path` is ignored by the repository's ignore rules.
fn is_ignored(repository: &Repository, path: &str, directory: bool) -> Result<bool> {
    let mut stack = repository.ignore()?.clone();
    stack.push_dir(BStr::new(""))?;
    let parts: Vec<&str> = path.split('/').collect();
    let mut prefix = String::new();
    for (at, part) in parts.iter().enumerate() {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        let last = at + 1 == parts.len();
        let kind = if !last || directory {
            rivet_core::PathKind::Directory
        } else {
            rivet_core::PathKind::File
        };
        if stack
            .check(BStr::new(prefix.as_bytes()), kind)?
            .is_ignored()
        {
            return Ok(true);
        }
        if !last {
            stack.push_dir(BStr::new(prefix.as_bytes()))?;
        }
    }
    Ok(false)
}

/// Clone `url` into `git_dir` with `dest` as its work tree, as `submodule add` does: the git
/// directory lives under `.git/modules`, `dest/.git` is a file pointing at it and the module
/// config records the work tree. `Err(code)` after printing git's two-line failure.
fn clone_submodule(
    dest: &Path,
    git_dir: &Path,
    url: &str,
    quiet: bool,
    depth: Option<&str>,
    single_branch: Option<bool>,
) -> std::result::Result<(), i32> {
    let failed = |message: String| {
        eprintln!("fatal: {message}");
        eprintln!(
            "fatal: clone of '{url}' into submodule path '{}' failed",
            dest.display()
        );
        let _ = std::fs::remove_dir_all(git_dir);
        let _ = std::fs::remove_dir_all(dest);
        128
    };
    if let Ok(parsed) = rivet_core::RemoteUrl::parse(BStr::new(url.as_bytes()))
        && parsed.scheme == rivet_core::remote::RemoteScheme::File
        && Repository::discover(Path::new(
            &String::from_utf8_lossy(&parsed.path).into_owned(),
        ))
        .is_err()
    {
        if url.contains("://") {
            if !quiet {
                eprintln!("Cloning into '{}'...", dest.display());
            }
            return Err(failed(format!(
                "'{}' does not appear to be a git repository\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.",
                url.strip_prefix("file://").unwrap_or(url)
            )));
        }
        return Err(failed(format!("repository '{url}' does not exist")));
    }
    if !quiet {
        eprintln!("Cloning into '{}'...", dest.display());
    }
    if depth.is_some() {
        if url.contains("://") {
            let _ = std::fs::remove_dir_all(git_dir);
            let _ = std::fs::remove_dir_all(dest);
            eprintln!(
                "fatal: submodule update: --depth with a file:// or network URL is not implemented"
            );
            return Err(128);
        }
        eprintln!("warning: --depth is ignored in local clones; use file:// instead.");
    }
    let setup = || -> Result<Repository> {
        std::fs::create_dir_all(dest)?;
        let options = rivet_core::InitOptions {
            bare: true,
            initial_branch: rivet_core::ops::default_branch_name()?,
            format: rivet_core::ObjectFormat::Sha1,
            work_tree: None,
        };
        let mut module = Repository::init(git_dir, &options)?;
        let config = module.config_mut();
        config.set(BStr::new("core.bare"), b"false")?;
        config.set(BStr::new("core.logallrefupdates"), b"true")?;
        let worktree = relative_path(git_dir, dest);
        config.set(
            BStr::new("core.worktree"),
            worktree.to_string_lossy().as_bytes(),
        )?;
        module.write_config()?;
        let link = relative_path(dest, git_dir);
        std::fs::write(dest.join(".git"), format!("gitdir: {}\n", link.display()))?;
        Ok(Repository::open_with(git_dir, Some(dest), None)?)
    };
    let result = setup().and_then(|mut module| {
        super::setup::populate_clone(
            &mut module,
            &super::setup::CloneArgs {
                repository: url.to_string(),
                directory: None,
                bare: false,
                mirror: false,
                branch: None,
                depth: None,
                local: false,
                tags: false,
                origin: "origin".to_string(),
                object_format: None,
            },
            false,
        )
        .map(|_| ())
    });
    // `--depth` implies `--single-branch` unless `--no-single-branch` came last: the clone's fetch
    // refspec names only the cloned branch. Narrowing is only honest when the source had a single
    // branch; anything else is refused.
    let narrow = single_branch.unwrap_or(depth.is_some());
    let result = result.and_then(|()| {
        if !narrow {
            return Ok(());
        }
        let mut module = Repository::open_with(git_dir, Some(dest), None)?;
        let tracked: Vec<String> = module
            .refs()
            .list(b"refs/remotes/origin/")?
            .into_iter()
            .map(|(name, _)| String::from_utf8_lossy(name.as_bytes()).into_owned())
            .filter(|name| name != "refs/remotes/origin/HEAD")
            .collect();
        let [only] = tracked.as_slice() else {
            anyhow::bail!(
                "submodule update: --single-branch (or --depth) on a source with several branches is not implemented"
            );
        };
        let branch = only.trim_start_matches("refs/remotes/origin/");
        module.config_mut().set(
            BStr::new("remote.origin.fetch"),
            format!("+refs/heads/{branch}:refs/remotes/origin/{branch}").as_bytes(),
        )?;
        module.write_config()?;
        Ok(())
    });
    match result {
        Ok(()) => {
            // Git's local-path clone reports `done.`; a `file://` one does not.
            if !quiet && !url.contains("://") {
                eprintln!("done.");
            }
            Ok(())
        }
        Err(error) => Err(failed(error.to_string())),
    }
}

/// The checkout `submodule add` runs after `clone --no-checkout`: `HEAD`'s tree, or with a branch
/// `git checkout -q -B <branch> origin/<branch>`.
fn checkout_branch(
    dest: &Path,
    git_dir: &Path,
    branch: Option<&str>,
    sm_path: &str,
) -> std::result::Result<(), i32> {
    let mut sub = Repository::open_with(git_dir, Some(dest), None).map_err(|e| {
        eprintln!("fatal: {e}");
        128
    })?;
    let target = match branch {
        None => sub.head().ok().and_then(|head| head.oid()),
        Some(branch) => {
            rivet_core::RefName::from_bytes(format!("refs/remotes/origin/{branch}").as_bytes())
                .ok()
                .and_then(|name| sub.refs().resolve_oid(&name).ok())
        }
    };
    let Some(oid) = target else {
        if let Some(branch) = branch {
            eprintln!(
                "fatal: 'origin/{branch}' is not a commit and a branch '{branch}' cannot be created from it"
            );
            eprintln!("fatal: unable to checkout submodule '{sm_path}'");
            return Err(128);
        }
        return Ok(());
    };
    let mut run = || -> Result<()> {
        if let Some(branch) = branch {
            let who = sub.committer_signature()?;
            let name = rivet_core::RefName::from_bytes(format!("refs/heads/{branch}").as_bytes())?;
            let from = match sub.head()? {
                rivet_core::HeadState::Attached { branch, .. } => branch.short_name().to_string(),
                _ => "HEAD".to_string(),
            };
            sub.set_ref_logged(
                &name,
                oid,
                &who,
                BStr::new(format!("branch: Created from origin/{branch}").as_bytes()),
            )?;
            let config = sub.config_mut();
            config.set(
                BStr::new(format!("branch.{branch}.remote").as_bytes()),
                b"origin",
            )?;
            config.set(
                BStr::new(format!("branch.{branch}.merge").as_bytes()),
                format!("refs/heads/{branch}").as_bytes(),
            )?;
            sub.write_config()?;
            sub.point_head(
                rivet_core::ops::HeadTarget::Branch(&name),
                BStr::new(format!("checkout: moving from {from} to {branch}").as_bytes()),
            )?;
        }
        let tree = sub.commit_tree(oid)?;
        sub.reset_hard_to_tree(tree)?;
        Ok(())
    };
    run().map_err(|e| {
        eprintln!("fatal: {e}");
        128
    })
}

/// A config value as `git config` writes it: quoted when it has leading or trailing blanks or a
/// character that would start a comment.
fn config_value(value: &str) -> String {
    let needs_quotes = value.starts_with([' ', '\t'])
        || value.ends_with([' ', '\t'])
        || value.contains(['#', ';', '"', '\\', '\n']);
    if !needs_quotes {
        return value.to_string();
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

fn submodule_update(global: &GlobalArgs, mut quiet: bool, args: &[String]) -> Result<i32> {
    let mut init = false;
    let mut recursive = false;
    let mut force = false;
    let mut no_fetch = false;
    let mut remote = false;
    let mut strategy_flag: Option<&'static str> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut only_paths = false;
    let mut depth: Option<String> = None;
    let mut single_branch: Option<bool> = None;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            _ if only_paths => paths.push(word.clone()),
            "-q" | "--quiet" => quiet = true,
            "--init" => init = true,
            "--recursive" => recursive = true,
            "-f" | "--force" => force = true,
            "-N" | "--no-fetch" => no_fetch = true,
            "--remote" => remote = true,
            "--checkout" => strategy_flag = Some("checkout"),
            "--merge" => strategy_flag = Some("merge"),
            "--rebase" => strategy_flag = Some("rebase"),
            "--depth" | "--jobs" | "-j" => {
                let Some(value) = words.next() else {
                    eprint!("{USAGE}");
                    return Ok(1);
                };
                if let Some(code) = take_int_option(word, value, &mut depth) {
                    return Ok(code);
                }
            }
            other if other.starts_with("--depth=") || other.starts_with("--jobs=") => {
                let (name, value) = other.split_once('=').unwrap_or((other, ""));
                if let Some(code) = take_int_option(name, value, &mut depth) {
                    return Ok(code);
                }
            }
            other if other.starts_with("-j") && other.len() > 2 && !other.starts_with("--") => {
                if let Some(code) = take_int_option("-j", &other[2..], &mut depth) {
                    return Ok(code);
                }
            }
            "--single-branch" => single_branch = Some(true),
            "--no-single-branch" => single_branch = Some(false),
            "--reference" | "--filter" | "--dissociate" => {
                anyhow::bail!("submodule update: {word} is not implemented")
            }
            "--recommend-shallow" | "--no-recommend-shallow" | "--progress" => {}
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
    let options = UpdateOptions {
        init,
        recursive,
        force,
        no_fetch,
        remote,
        strategy_flag,
        quiet,
        depth,
        single_branch,
    };
    update_in(&mut repository, &root, &prefix, "", &paths, &options)
}

/// Validate the integer value of `--depth` / `--jobs` the way git's option parser does (optional
/// `k`/`m`/`g` suffix) and record `--depth`. `Some(code)` is the exit status after the error.
fn take_int_option(name: &str, value: &str, depth: &mut Option<String>) -> Option<i32> {
    let digits = value.trim_end_matches(['k', 'K', 'm', 'M', 'g', 'G']);
    let suffix = value.len() - digits.len();
    let numeric = digits.strip_prefix(['-', '+']).unwrap_or(digits);
    if suffix > 1 || numeric.is_empty() || !numeric.bytes().all(|b| b.is_ascii_digit()) {
        let shown = if name == "--depth" { "depth" } else { "jobs" };
        eprintln!("error: option `{shown}' expects an integer value with an optional k/m/g suffix");
        return Some(129);
    }
    if name == "--depth" {
        *depth = Some(value.to_string());
    }
    None
}

struct UpdateOptions {
    init: bool,
    recursive: bool,
    force: bool,
    no_fetch: bool,
    remote: bool,
    /// `--checkout` / `--merge` / `--rebase`: wins over `submodule.<name>.update`.
    strategy_flag: Option<&'static str>,
    quiet: bool,
    /// `--depth <n>`: passed verbatim to the submodule's own clone.
    depth: Option<String>,
    /// `--single-branch` / `--no-single-branch`, the last one given.
    single_branch: Option<bool>,
}

/// Bring the submodules of `repository` to the commits its index records.
fn update_in(
    repository: &mut Repository,
    root: &Path,
    prefix: &str,
    shown_prefix: &str,
    paths: &[String],
    options: &UpdateOptions,
) -> Result<i32> {
    if options.init {
        let code = init_in(repository, root, prefix, shown_prefix, paths, options.quiet)?;
        if code != 0 {
            return Ok(code);
        }
    }
    let links = match module_list(repository, prefix, paths) {
        Ok(links) => links,
        Err(code) => return Ok(code),
    };
    let gitmodules = gitmodules_config(repository, root);
    if let Some(message) = check_update_settings(&gitmodules) {
        eprintln!("fatal: {message}");
        return Ok(128);
    }
    let recorded: Vec<(BString, ObjectId)> = repository
        .index()
        .entries()
        .iter()
        .filter(|e| e.mode == EntryMode::Commit && !e.is_unmerged())
        .map(|e| (e.path.clone(), e.oid))
        .collect();
    for link in links {
        let display = format!("{shown_prefix}{}", display_path(prefix, link.path.as_ref()));
        let Some(name) = name_of(&gitmodules, link.path.as_ref()) else {
            eprintln!("fatal: no submodule mapping found in .gitmodules for path '{display}'");
            return Ok(128);
        };
        let Some(mut oid) = recorded
            .iter()
            .find(|(path, _)| *path == link.path)
            .map(|(_, oid)| *oid)
        else {
            continue;
        };
        if !is_active(repository, name.as_ref(), link.path.as_ref()) {
            if !paths.is_empty() {
                eprintln!("Submodule path '{display}' not initialized");
                eprintln!("Maybe you want to use 'update --init'?");
            }
            continue;
        }
        let strategy = options.strategy_flag.map(str::to_string).or_else(|| {
            repository
                .config_value(key_of(name.as_ref(), "update").as_ref())
                .ok()
                .flatten()
                .map(|v| v.to_string())
                .or_else(|| setting(&gitmodules, name.as_ref(), "update").map(|v| v.to_string()))
        });
        let merge = strategy.as_deref() == Some("merge");
        let rebase = strategy.as_deref() == Some("rebase");
        match strategy.as_deref() {
            None | Some("checkout") | Some("merge") | Some("rebase") => {}
            Some("none") => {
                eprintln!("Skipping submodule '{display}'");
                continue;
            }
            Some(other) => {
                anyhow::bail!("submodule update: update strategy '{other}' is not implemented")
            }
        }
        let dest = root.join(link.path.to_os_str_lossy());
        let git_dir = repository
            .common_dir()
            .join("modules")
            .join(name.to_os_str_lossy());
        let mut fresh = false;
        if !dest.join(".git").exists() {
            fresh = true;
            if git_dir.exists() {
                connect_module(&dest, &git_dir)?;
            } else {
                let Some(url) = repository.config_value(key_of(name.as_ref(), "url").as_ref())?
                else {
                    eprintln!("Submodule path '{display}' not initialized");
                    eprintln!("Maybe you want to use 'update --init'?");
                    continue;
                };
                if let Err(code) = clone_submodule(
                    &dest,
                    &git_dir,
                    &url.to_string(),
                    options.quiet,
                    options.depth.as_deref(),
                    options.single_branch,
                ) {
                    return Ok(code);
                }
            }
        }
        let mut sub = Repository::open_with(&git_dir, Some(&dest), None)
            .or_else(|_| Repository::open_or_discover(&dest))?;
        let mut remote_fetched = false;
        if options.remote {
            // `--remote` follows the submodule's configured branch on its own remote, `HEAD` by default.
            let branch = match setting(&gitmodules, name.as_ref(), "branch") {
                None => "HEAD".to_string(),
                Some(b) if b.as_slice() == b"." => match repository.head()? {
                    rivet_core::HeadState::Attached { branch, .. } => {
                        branch.short_name().to_string()
                    }
                    _ => anyhow::bail!(
                        "submodule update: --remote with branch '.' needs an attached HEAD"
                    ),
                },
                Some(b) => b.to_string(),
            };
            if !options.no_fetch {
                // Git's own fetch here ignores `-q`: the ref lines and the reflog reason stay.
                fetch_origin(&mut sub, false)?;
                remote_fetched = true;
            }
            let full = format!("refs/remotes/origin/{branch}");
            match super::resolve_commit(&sub, &full) {
                Ok(found) => oid = found,
                Err(_) => {
                    eprintln!(
                        "fatal: Unable to find {full} revision in submodule path '{display}'"
                    );
                    return Ok(128);
                }
            }
        }
        let current = sub.head().ok().and_then(|head| head.oid());
        if fresh || current != Some(oid) || options.force {
            if !sub.odb().contains(&oid) && !options.no_fetch && !remote_fetched {
                fetch_origin(&mut sub, options.quiet)?;
            }
            if !sub.odb().contains(&oid) {
                if options.no_fetch {
                    eprintln!("fatal: unable to read tree ({oid})");
                    eprintln!("fatal: Unable to checkout '{oid}' in submodule path '{display}'");
                } else {
                    eprintln!(
                        "fatal: Fetched in submodule path '{display}', but it did not contain {oid}. Direct fetching of that commit failed."
                    );
                }
                return Ok(128);
            }
            if rebase {
                // Git runs `git rebase [-q] <oid>` inside the submodule.
                let inner = GlobalArgs {
                    repo: Some(dest.clone()),
                    verbose: false,
                    quiet: options.quiet,
                    no_pager: true,
                };
                let args = super::rebase::RebaseArgs {
                    upstream: Some(oid.to_string()),
                    ..Default::default()
                };
                if super::rebase::run_rebase(&inner, &args)? != 0 {
                    eprintln!("fatal: Unable to rebase '{oid}' in submodule path '{display}'");
                    return Ok(128);
                }
                sub = Repository::open_with(&git_dir, Some(&dest), None)
                    .or_else(|_| Repository::open_or_discover(&dest))?;
                if !options.quiet {
                    println!("Submodule path '{display}': rebased into '{oid}'");
                }
            } else if merge {
                // Git runs `git merge [-q] <oid>` inside the submodule.
                let inner = GlobalArgs {
                    repo: Some(dest.clone()),
                    verbose: false,
                    quiet: options.quiet,
                    no_pager: true,
                };
                let args = super::branching::MergeArgs {
                    commit: Some(oid.to_string()),
                    ..Default::default()
                };
                if super::branching::run_merge(&inner, &args)? != 0 {
                    eprintln!("fatal: Unable to merge '{oid}' in submodule path '{display}'");
                    return Ok(128);
                }
                sub = Repository::open_with(&git_dir, Some(&dest), None)
                    .or_else(|_| Repository::open_or_discover(&dest))?;
                if !options.quiet {
                    println!("Submodule path '{display}': merged in '{oid}'");
                }
            } else if let Err(error) = checkout_detached(&mut sub, oid, options.force, fresh) {
                match error.downcast::<rivet_core::Error>() {
                    Ok(core) => {
                        if let Err(other) =
                            super::branching::report_checkout_refusal(core, "checkout")
                        {
                            eprintln!("error: {other}");
                        }
                    }
                    Err(other) => eprintln!("error: {other}"),
                }
                eprintln!("fatal: Unable to checkout '{oid}' in submodule path '{display}'");
                return Ok(1);
            } else if !options.quiet {
                println!("Submodule path '{display}': checked out '{oid}'");
            }
        }
        if options.recursive {
            let nested = format!("{display}/");
            let sub_root = sub.worktree_path().map(Path::to_path_buf).unwrap_or(dest);
            let code = update_in(&mut sub, &sub_root, "", &nested, &[], options)?;
            if code != 0 {
                return Ok(code);
            }
        }
    }
    Ok(0)
}

/// Re-attach an existing module directory to a work tree that lost its `.git` file.
fn connect_module(dest: &Path, git_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let link = relative_path(dest, git_dir);
    std::fs::write(dest.join(".git"), format!("gitdir: {}\n", link.display()))?;
    let mut module = Repository::open_with(git_dir, None, None)?;
    let worktree = relative_path(git_dir, dest);
    module.config_mut().set(
        BStr::new("core.worktree"),
        worktree.to_string_lossy().as_bytes(),
    )?;
    module.write_config()?;
    Ok(())
}

/// `git fetch` in a submodule, from its default remote.
fn fetch_origin(sub: &mut Repository, quiet: bool) -> Result<()> {
    use rivet_core::remote_ops::{FetchRequest, TagMode};
    let request = FetchRequest {
        refspecs: Vec::new(),
        tags: TagMode::Auto,
        prune: false,
        force: false,
        dry_run: false,
        write_fetch_head: true,
        pack_refs: false,
        action: if quiet { "fetch --quiet" } else { "fetch" }.to_string(),
    };
    let report = sub.fetch_remote("origin", &request)?;
    super::remote::print_fetch_report(&report, quiet);
    Ok(())
}

/// `git checkout -q <oid>` in a submodule: detach `HEAD` there and bring the work tree along.
fn checkout_detached(sub: &mut Repository, oid: ObjectId, force: bool, fresh: bool) -> Result<()> {
    let from = match sub.head()? {
        rivet_core::HeadState::Attached { branch, .. } => branch.short_name().to_string(),
        rivet_core::HeadState::Detached { oid, .. } => oid.to_string(),
        rivet_core::HeadState::Unborn { branch } => branch.short_name().to_string(),
    };
    let tree = sub.commit_tree(oid)?;
    // A just-cloned module has no index and no files yet; a forced update throws local changes away.
    if fresh || sub.head()?.oid().is_none() {
        sub.reset_hard_to_tree(tree)?;
    } else {
        sub.checkout_tree(tree, force)?;
    }
    // Staying on the commit `HEAD` already detaches at writes no reflog line.
    if matches!(sub.head()?, rivet_core::HeadState::Detached { oid: at, .. } if at == oid) {
        return Ok(());
    }
    sub.point_head(
        rivet_core::ops::HeadTarget::Detached(oid),
        BStr::new(format!("checkout: moving from {from} to {oid}").as_bytes()),
    )?;
    Ok(())
}

const USAGE_DEINIT: &str =
    "usage: git submodule deinit [--quiet] [-f | --force] [--all | [--] [<path>...]]

    -q, --[no-]quiet      suppress submodule status output
    -f, --[no-]force      remove submodule working trees even if they contain local changes
    --[no-]all            unregister all submodules

";

fn submodule_deinit(global: &GlobalArgs, mut quiet: bool, args: &[String]) -> Result<i32> {
    let mut force = false;
    let mut all = false;
    let mut paths: Vec<String> = Vec::new();
    let mut only_paths = false;
    for word in args {
        match word.as_str() {
            _ if only_paths => paths.push(word.clone()),
            "-q" | "--quiet" => quiet = true,
            "-f" | "--force" => force = true,
            "--all" => all = true,
            "--" => only_paths = true,
            other if other.starts_with('-') => {
                eprint!("{USAGE}");
                return Ok(1);
            }
            other => paths.push(other.to_string()),
        }
    }
    if all && !paths.is_empty() {
        eprintln!("error: pathspec and --all are incompatible");
        eprint!("{USAGE_DEINIT}");
        return Ok(129);
    }
    if !all && paths.is_empty() {
        eprintln!("fatal: Use '--all' if you really want to deinitialize all submodules");
        return Ok(128);
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
    for link in links {
        let display = display_path(&prefix, link.path.as_ref());
        let name = name_of(&gitmodules, link.path.as_ref()).unwrap_or_else(|| link.path.clone());
        let dir = root.join(link.path.to_os_str_lossy());
        if !force && let Some(problem) = removal_problem(&repository, &link.path, &dir) {
            eprintln!("error: {problem}");
            eprintln!("    {}", link.path);
            eprintln!("(use --cached to keep the file, or -f to force removal)");
            eprintln!(
                "fatal: Submodule work tree '{display}' contains local modifications; use '-f' to discard them"
            );
            return Ok(128);
        }
        // The module directory stops pointing at a work tree that is about to be emptied.
        let git_dir = repository
            .common_dir()
            .join("modules")
            .join(name.to_os_str_lossy());
        let unset = Repository::open_with(&git_dir, None, None)
            .ok()
            .and_then(|mut module| {
                let removed = module
                    .config_mut()
                    .remove(BStr::new("core.worktree"))
                    .ok()?;
                removed.then(|| module.write_config().ok()).flatten()
            });
        if unset.is_none() {
            if !git_dir.exists() {
                let from = std::env::current_dir().unwrap_or_else(|_| root.clone());
                eprintln!(
                    "error: could not lock config file {}: No such file or directory",
                    relative_path(&canonical(&from), &canonical(&git_dir.join("config"))).display()
                );
            }
            eprintln!("warning: Could not unset core.worktree setting in submodule '{name}'");
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        if !quiet {
            println!("Cleared directory '{display}'");
        }
        let removed = repository
            .config_mut()
            .remove_section(BStr::new("submodule"), Some(name.as_ref()));
        if removed {
            repository.write_config()?;
            if !quiet {
                let url = setting(&gitmodules, name.as_ref(), "url").unwrap_or_default();
                println!("Submodule '{name}' ({url}) unregistered for path '{display}'");
            }
        }
    }
    Ok(0)
}

/// Why `git rm` would refuse to remove the submodule at `path` (its work tree is `dir`).
fn removal_problem(repository: &Repository, path: &BString, dir: &Path) -> Option<&'static str> {
    let in_head = repository
        .head_commit()
        .ok()
        .flatten()
        .and_then(|commit| repository.commit_tree(commit).ok())
        .and_then(|tree| rivet_core::ops::flatten_tree(repository.odb(), tree).ok())
        .and_then(|files| files.get(path).map(|(_, oid)| *oid));
    let staged = repository
        .index()
        .entry(path.as_ref())
        .is_some_and(|entry| in_head != Some(entry.oid));
    if staged {
        return Some("the following file has changes staged in the index:");
    }
    if !dir.join(".git").exists() {
        return None;
    }
    let mut sub = Repository::open_or_discover(dir).ok()?;
    let recorded = repository.index().entry(path.as_ref()).map(|e| e.oid);
    let moved = sub.head().ok().and_then(|h| h.oid()) != recorded;
    let dirty = sub
        .status(&rivet_core::StatusOptions::new())
        .map(|s| !s.is_clean())
        .unwrap_or(false);
    (moved || dirty).then_some("the following file has local modifications:")
}

fn submodule_foreach(global: &GlobalArgs, mut quiet: bool, args: &[String]) -> Result<i32> {
    let mut recursive = false;
    let mut at = 0;
    while let Some(word) = args.get(at) {
        match word.as_str() {
            "-q" | "--quiet" => quiet = true,
            "--recursive" => recursive = true,
            other if other.starts_with('-') => {
                eprint!("{USAGE}");
                return Ok(1);
            }
            _ => break,
        }
        at += 1;
    }
    let command = &args[at..];
    let mut repository = super::open(global)?;
    let Some(root) = repository.worktree_path().map(Path::to_path_buf) else {
        return Ok(0);
    };
    let prefix = cwd_prefix(&root);
    foreach_in(
        &mut repository,
        &root,
        &prefix,
        "",
        command,
        quiet,
        recursive,
    )
}

/// Run `command` (first word: the shell text, the rest: its arguments) in every checked-out
/// submodule of `repository`.
fn foreach_in(
    repository: &mut Repository,
    root: &Path,
    prefix: &str,
    shown_prefix: &str,
    command: &[String],
    quiet: bool,
    recursive: bool,
) -> Result<i32> {
    use std::io::Write as _;

    let links = match module_list(repository, prefix, &[]) {
        Ok(links) => links,
        Err(code) => return Ok(code),
    };
    let gitmodules = gitmodules_config(repository, root);
    let recorded: Vec<(BString, ObjectId)> = repository
        .index()
        .entries()
        .iter()
        .filter(|e| e.mode == EntryMode::Commit && !e.is_unmerged())
        .map(|e| (e.path.clone(), e.oid))
        .collect();
    for link in links {
        let dest = root.join(link.path.to_os_str_lossy());
        if !dest.join(".git").exists() {
            continue;
        }
        let Ok(mut sub) = Repository::open_or_discover(&dest) else {
            continue;
        };
        let Some(oid) = recorded
            .iter()
            .find(|(path, _)| *path == link.path)
            .map(|(_, oid)| *oid)
        else {
            continue;
        };
        let name = name_of(&gitmodules, link.path.as_ref()).unwrap_or_else(|| link.path.clone());
        let display = format!("{shown_prefix}{}", display_path(prefix, link.path.as_ref()));
        if !quiet {
            println!("Entering '{display}'");
        }
        std::io::stdout().flush()?;
        let script = match command.first() {
            Some(text) => format!("path=\"$sm_path\"; {text} \"$@\""),
            None => "path=\"$sm_path\"".to_string(),
        };
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .arg(command.first().map(String::as_str).unwrap_or("sh"))
            .args(command.iter().skip(1))
            .current_dir(&dest)
            .env("name", name.to_string())
            .env("sm_path", link.path.to_string())
            .env("displaypath", &display)
            .env("sha1", oid.to_string())
            .env("toplevel", canonical(root))
            .env("GIT_DIR", ".git")
            .env_remove("GIT_WORK_TREE")
            .status();
        if !status.is_ok_and(|s| s.success()) {
            eprintln!("fatal: run_command returned non-zero status for {display}\n.");
            return Ok(128);
        }
        if recursive {
            let nested = format!("{display}/");
            let sub_root = sub.worktree_path().map(Path::to_path_buf).unwrap_or(dest);
            let code = foreach_in(&mut sub, &sub_root, "", &nested, command, quiet, true)?;
            if code != 0 {
                eprintln!(
                    "fatal: run_command returned non-zero status while recursing in the nested submodules of {display}\n."
                );
                return Ok(128);
            }
        }
    }
    Ok(0)
}

const USAGE_SET_BRANCH: &str = "usage: git submodule set-branch [-q|--quiet] (-d|--default) <path>
   or: git submodule set-branch [-q|--quiet] (-b|--branch) <branch> <path>

    -d, --[no-]default    set the default tracking branch to master
    -b, --[no-]branch <branch>
                          set the default tracking branch

";

const USAGE_SET_URL: &str = "usage: git submodule set-url [--quiet] <path> <newurl>

    -q, --[no-]quiet      suppress output for setting url of a submodule

";

/// Set (`Some`), replace or remove (`None`) `key` in the `[submodule "<name>"]` section of a
/// `.gitmodules` text, leaving everything else of the file as it is. `false` when `key` was to
/// be removed and was not there.
fn edit_gitmodules(text: &mut String, name: &str, key: &str, value: Option<&str>) -> bool {
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    let header = format!("[submodule \"{escaped}\"]");
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let Some(start) = lines.iter().position(|l| l.trim() == header) else {
        // A key that is not there cannot be removed; a new one starts its section at the end.
        let Some(value) = value else {
            return false;
        };
        if let Some(last) = lines.last_mut()
            && !last.ends_with('\n')
        {
            last.push('\n');
        }
        lines.push(format!("{header}\n"));
        lines.push(format!("\t{key} = {}\n", config_value(value)));
        *text = lines.concat();
        return true;
    };
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |i| start + 1 + i);
    let found = (start + 1..end).find(|&i| {
        let line = lines[i].trim_start();
        line.split('=')
            .next()
            .is_some_and(|k| k.trim().eq_ignore_ascii_case(key))
            && line.contains('=')
    });
    match (found, value) {
        (Some(i), Some(value)) => lines[i] = format!("\t{key} = {}\n", config_value(value)),
        (Some(i), None) => {
            lines.remove(i);
        }
        (None, Some(value)) => {
            let last = (start + 1..end)
                .rev()
                .find(|&i| {
                    let line = lines[i].trim();
                    !line.is_empty() && !line.starts_with('#') && !line.starts_with(';')
                })
                .unwrap_or(start);
            if !lines[last].ends_with('\n') {
                lines[last].push('\n');
            }
            lines.insert(last + 1, format!("\t{key} = {}\n", config_value(value)));
        }
        (None, None) => return false,
    }
    *text = lines.concat();
    true
}

/// The submodule name for `path` (given relative to the cwd), or git's fatal text.
fn gitmodules_name_for(
    repository: &Repository,
    root: &Path,
    typed: &str,
) -> std::result::Result<BString, String> {
    let prefix = cwd_prefix(root);
    let wanted = from_root(&prefix, typed.trim_end_matches('/'));
    let gitmodules = gitmodules_config(repository, root);
    name_of(&gitmodules, BStr::new(wanted.as_bytes()))
        .ok_or_else(|| format!("no submodule mapping found in .gitmodules for path '{typed}'"))
}

fn submodule_set_branch(global: &GlobalArgs, _quiet: bool, args: &[String]) -> Result<i32> {
    let mut default = false;
    let mut branch: Option<String> = None;
    let mut positional: Vec<&String> = Vec::new();
    let mut iter = args.iter();
    let mut only_paths = false;
    while let Some(word) = iter.next() {
        match word.as_str() {
            _ if only_paths => positional.push(word),
            "-q" | "--quiet" => {}
            "-d" | "--default" => default = true,
            "-b" | "--branch" => match iter.next() {
                Some(value) => branch = Some(value.clone()),
                None => {
                    eprint!("{USAGE}");
                    return Ok(1);
                }
            },
            w if w.starts_with("--branch=") => branch = Some(w["--branch=".len()..].to_string()),
            "--" => only_paths = true,
            w if w.starts_with('-') && w.len() > 1 => {
                eprint!("{USAGE}");
                return Ok(1);
            }
            _ => positional.push(word),
        }
    }
    if positional.len() != 1 {
        eprint!("{USAGE_SET_BRANCH}");
        return Ok(129);
    }
    if default && branch.is_some() {
        eprintln!("fatal: options '--branch' and '--default' cannot be used together");
        return Ok(128);
    }
    if !default && branch.is_none() {
        eprintln!("fatal: --branch or --default required");
        return Ok(128);
    }
    let repository = super::open(global)?;
    let Some(root) = repository.worktree_path().map(Path::to_path_buf) else {
        return Ok(0);
    };
    let name = match gitmodules_name_for(&repository, &root, positional[0]) {
        Ok(name) => name,
        Err(message) => {
            eprintln!("fatal: {message}");
            return Ok(128);
        }
    };
    let file = root.join(".gitmodules");
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    let changed = edit_gitmodules(&mut text, &name.to_string(), "branch", branch.as_deref());
    if !changed {
        // `git config --unset` of a key that is not there.
        return Ok(1);
    }
    std::fs::write(&file, text)?;
    Ok(0)
}

fn submodule_set_url(global: &GlobalArgs, quiet: bool, args: &[String]) -> Result<i32> {
    let mut quiet = quiet;
    let mut positional: Vec<&String> = Vec::new();
    let mut only_paths = false;
    for word in args {
        match word.as_str() {
            _ if only_paths => positional.push(word),
            "-q" | "--quiet" => quiet = true,
            "--" => only_paths = true,
            w if w.starts_with('-') && w.len() > 1 => {
                eprint!("{USAGE}");
                return Ok(1);
            }
            _ => positional.push(word),
        }
    }
    if positional.len() != 2 {
        eprint!("{USAGE_SET_URL}");
        return Ok(129);
    }
    let mut repository = super::open(global)?;
    let Some(root) = repository.worktree_path().map(Path::to_path_buf) else {
        return Ok(0);
    };
    let name = match gitmodules_name_for(&repository, &root, positional[0]) {
        Ok(name) => name,
        Err(message) => {
            eprintln!("fatal: {message}");
            return Ok(128);
        }
    };
    let file = root.join(".gitmodules");
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    edit_gitmodules(&mut text, &name.to_string(), "url", Some(positional[1]));
    std::fs::write(&file, text)?;
    // Then the url is carried into the config the way `submodule sync` does it.
    let prefix = cwd_prefix(&root);
    let known = KnownUrl {
        path: BString::from(from_root(&prefix, positional[0].trim_end_matches('/'))),
        name,
        url: positional[1].clone(),
    };
    sync_in(
        &mut repository,
        &root,
        &prefix,
        "",
        &[positional[0].clone()],
        quiet,
        false,
        Some(&known),
    )
}

const USAGE_ABSORB: &str = "usage: git submodule absorbgitdirs [<options>] [<path>...]

";

fn submodule_absorbgitdirs(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut paths: Vec<String> = Vec::new();
    let mut only_paths = false;
    for word in args {
        match word.as_str() {
            _ if only_paths => paths.push(word.clone()),
            "--" => only_paths = true,
            w if w.starts_with("--") => {
                eprintln!("error: unknown option `{}'", &w[2..]);
                eprint!("{USAGE_ABSORB}");
                return Ok(129);
            }
            w if w.starts_with('-') && w.len() > 1 => {
                eprintln!("error: unknown switch `{}'", &w[1..2]);
                eprint!("{USAGE_ABSORB}");
                return Ok(129);
            }
            other => paths.push(other.to_string()),
        }
    }
    let repository = super::open(global)?;
    let Some(root) = repository.worktree_path().map(Path::to_path_buf) else {
        return Ok(0);
    };
    let prefix = cwd_prefix(&root);
    absorb_in(&repository, &root, &prefix, "", &paths)
}

/// Move the embedded git directories of the submodules of `repository` under its own
/// `.git/modules`, and do the same for the submodules they contain.
fn absorb_in(
    repository: &Repository,
    root: &Path,
    prefix: &str,
    shown_prefix: &str,
    paths: &[String],
) -> Result<i32> {
    let links = match module_list(repository, prefix, paths) {
        Ok(links) => links,
        Err(code) => return Ok(code),
    };
    let gitmodules = gitmodules_config(repository, root);
    for link in links {
        let display = format!("{shown_prefix}{}", display_path(prefix, link.path.as_ref()));
        let dir = root.join(link.path.to_os_str_lossy());
        let dot_git = dir.join(".git");
        if !dot_git.exists() {
            continue;
        }
        let Some(name) = name_of(&gitmodules, link.path.as_ref()) else {
            eprintln!("fatal: could not lookup name for submodule '{display}'");
            return Ok(128);
        };
        if dot_git.is_dir() {
            let dest = repository
                .common_dir()
                .join("modules")
                .join(name.to_os_str_lossy());
            if dest.exists() {
                anyhow::bail!(
                    "submodule absorbgitdirs: the module directory '{}' already exists",
                    dest.display()
                );
            }
            eprintln!(
                "Migrating git directory of '{display}' from\n'{}' to\n'{}'",
                dot_git.display(),
                dest.display()
            );
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&dot_git, &dest)?;
            std::fs::write(
                &dot_git,
                format!("gitdir: {}\n", relative_path(&dir, &dest).display()),
            )?;
            let mut module = Repository::open_with(&dest, None, None)?;
            module.config_mut().set(
                BStr::new("core.worktree"),
                relative_path(&dest, &dir).to_string_lossy().as_bytes(),
            )?;
            module.write_config()?;
        }
        // The submodules inside it, whether it was just absorbed or already was.
        if let Ok(sub) = Repository::open_or_discover(&dir) {
            let sub_root = sub.worktree_path().map(Path::to_path_buf).unwrap_or(dir);
            let nested = format!("{display}/");
            let code = absorb_in(&sub, &sub_root, "", &nested, &[])?;
            if code != 0 {
                return Ok(code);
            }
        }
    }
    Ok(0)
}

/// One side of a changed path in `summary`: nothing, a gitlink, or anything else.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    None,
    Gitlink(ObjectId),
    Other(ObjectId),
}

impl Side {
    fn of(mode: EntryMode, oid: ObjectId) -> Self {
        if mode == EntryMode::Commit {
            Side::Gitlink(oid)
        } else {
            Side::Other(oid)
        }
    }

    fn is_gitlink(self) -> bool {
        matches!(self, Side::Gitlink(_))
    }
}

fn submodule_summary(global: &GlobalArgs, args: &[String]) -> Result<i32> {
    let mut cached = false;
    let mut files = false;
    let mut limit: i64 = -1;
    let mut rest: Vec<&String> = Vec::new();
    let mut iter = args.iter();
    while let Some(word) = iter.next() {
        match word.as_str() {
            "--cached" => cached = true,
            "--files" => files = true,
            "--for-status" => {}
            "-n" | "--summary-limit" => match iter.next() {
                Some(value) => match value.parse::<i64>() {
                    Ok(n) => limit = n,
                    Err(_) => {
                        eprintln!(
                            "error: option `summary-limit' expects an integer value with an optional k/m/g suffix"
                        );
                        return Ok(129);
                    }
                },
                None => {
                    eprint!("{USAGE}");
                    return Ok(1);
                }
            },
            w if w.starts_with("--summary-limit=") => {
                match w["--summary-limit=".len()..].parse::<i64>() {
                    Ok(n) => limit = n,
                    Err(_) => {
                        eprintln!(
                            "error: option `summary-limit' expects an integer value with an optional k/m/g suffix"
                        );
                        return Ok(129);
                    }
                }
            }
            "--" => {
                rest.extend(iter.by_ref());
                break;
            }
            w if w.starts_with('-') && w.len() > 1 => {
                eprint!("{USAGE}");
                return Ok(1);
            }
            _ => {
                rest.push(word);
                rest.extend(iter.by_ref());
                break;
            }
        }
    }
    if cached && files {
        eprintln!("fatal: options '--cached' and '--files' cannot be used together");
        return Ok(128);
    }
    let repository = super::open(global)?;
    let Some(root) = repository.worktree_path().map(Path::to_path_buf) else {
        return Ok(0);
    };
    // The first word is the commit to compare with when it names one, else a path.
    let mut head_spec = "HEAD".to_string();
    let mut paths: Vec<String> = Vec::new();
    let mut words = rest.into_iter().peekable();
    if let Some(first) = words.peek()
        && first.as_str() != "--"
        && super::resolve_commit(&repository, first).is_ok()
    {
        head_spec = (*first).clone();
        words.next();
    }
    if words.peek().is_some_and(|w| w.as_str() == "--") {
        words.next();
    }
    paths.extend(words.cloned());
    if limit == 0 {
        return Ok(0);
    }
    let prefix = cwd_prefix(&root);
    let specs: Vec<String> = paths.iter().map(|p| from_root(&prefix, p)).collect();

    let flatten = |tree: ObjectId| rivet_core::ops::flatten_tree(repository.odb(), tree);
    let head_files = match super::resolve_commit(&repository, &head_spec) {
        Ok(commit) => flatten(repository.commit_tree(commit)?)?,
        Err(_) => std::collections::BTreeMap::new(),
    };
    let index_files: std::collections::BTreeMap<BString, (EntryMode, ObjectId)> = repository
        .index()
        .entries()
        .iter()
        .filter(|e| !e.is_unmerged())
        .map(|e| (e.path.clone(), (e.mode, e.oid)))
        .collect();

    // The submodule's own `HEAD`, which is what the work tree holds of a gitlink.
    let worktree_head = |path: &BString, fallback: ObjectId| -> ObjectId {
        let dir = root.join(path.to_os_str_lossy());
        if !dir.join(".git").exists() {
            return fallback;
        }
        Repository::open_or_discover(&dir)
            .ok()
            .and_then(|sub| sub.head().ok()?.oid())
            .unwrap_or(fallback)
    };

    let mut all_paths: std::collections::BTreeSet<&BString> = std::collections::BTreeSet::new();
    all_paths.extend(head_files.keys());
    all_paths.extend(index_files.keys());
    let mut out = String::new();
    for path in all_paths {
        if !specs.is_empty()
            && !specs.iter().any(|spec| {
                rivet_core::ops::pathspec_matches(path.as_ref(), BStr::new(spec.as_bytes()))
            })
        {
            continue;
        }
        let in_head = head_files.get(path).map(|(m, o)| Side::of(*m, *o));
        let in_index = index_files.get(path).map(|(m, o)| Side::of(*m, *o));
        let (src, dst) = if files {
            let dst = match in_index {
                Some(Side::Gitlink(oid)) => Some(Side::Gitlink(worktree_head(path, oid))),
                other => other,
            };
            (in_index, dst)
        } else if cached {
            (in_head, in_index)
        } else {
            let dst = match in_index {
                Some(Side::Gitlink(oid)) => Some(Side::Gitlink(worktree_head(path, oid))),
                other => other,
            };
            (in_head, dst)
        };
        let (src, dst) = (src.unwrap_or(Side::None), dst.unwrap_or(Side::None));
        if src == dst || (!src.is_gitlink() && !dst.is_gitlink()) {
            continue;
        }
        if files && (src == Side::None || dst == Side::None) {
            continue;
        }
        // A submodule that is not checked out has nothing to compare, unless it is being removed
        // or replaced by a plain file.
        let populated = root.join(path.to_os_str_lossy()).join(".git").exists();
        if !populated && dst.is_gitlink() && !matches!(src, Side::Other(_)) {
            continue;
        }
        out.push_str(&summary_entry(
            &repository,
            &root,
            &display_path(&prefix, path.as_ref()),
            path,
            src,
            dst,
            limit,
        )?);
    }
    print!("{out}");
    Ok(0)
}

/// The text `submodule summary` prints for one changed gitlink.
fn summary_entry(
    repository: &Repository,
    root: &Path,
    display: &str,
    path: &BString,
    src: Side,
    dst: Side,
    limit: i64,
) -> Result<String> {
    use rivet_core::revwalk::{SortOrder, WalkOptions, walk_oids};

    let dir = root.join(path.to_os_str_lossy());
    let sub = dir
        .join(".git")
        .exists()
        .then(|| Repository::open_or_discover(&dir).ok())
        .flatten();
    let zero = "0000000".to_string();
    // An abbreviated name in the submodule when it has the commit, else the superproject's.
    let name_of_side = |side: Side| -> (String, bool) {
        match side {
            Side::None => (zero.clone(), false),
            Side::Other(oid) => (super::history::abbrev(repository, &oid, false), false),
            Side::Gitlink(oid) => match &sub {
                Some(sub) if sub.odb().contains(&oid) => {
                    (super::history::abbrev(sub, &oid, false), false)
                }
                _ => (super::history::abbrev(repository, &oid, false), true),
            },
        }
    };
    let (src_name, missing_src) = name_of_side(src);
    let (dst_name, missing_dst) = name_of_side(dst);
    let shown = path.to_string();
    if matches!(
        (src, dst),
        (Side::Gitlink(_), Side::Other(_)) | (Side::Other(_), Side::Gitlink(_))
    ) {
        // Git prints the object types here, and complains about a `cd` into what is now a file.
        let kind = |side: Side| {
            if side.is_gitlink() {
                "submodule"
            } else {
                "blob"
            }
        };
        if dir.is_file() {
            eprintln!("fatal: exec 'rev-parse': cd to '{shown}' failed: Not a directory");
        }
        return Ok(format!(
            "* {display} {src_name}({})->{dst_name}({}):\n\n",
            kind(src),
            kind(dst)
        ));
    }
    let mut text = format!("* {display} {src_name}...{dst_name}");
    if missing_src || missing_dst {
        text.push_str(":\n");
        text.push_str(&match (missing_src, missing_dst) {
            (true, false) => format!("  Warn: {shown} doesn't contain commit {src_name}\n"),
            (false, true) => format!("  Warn: {shown} doesn't contain commit {dst_name}\n"),
            _ => format!("  Warn: {shown} doesn't contain commits {src_name} and {dst_name}\n"),
        });
        text.push('\n');
        return Ok(text);
    }
    let Some(sub) = sub else {
        text.push_str(":\n\n");
        return Ok(text);
    };
    let mut options = WalkOptions::new();
    options.first_parent(true);
    options.sort(SortOrder::Date);
    let mut lines: Vec<(i64, char, String)> = Vec::new();
    let subject_and_date = |oid: &ObjectId| -> Result<(i64, String)> {
        let object = sub.odb().read_typed(oid, rivet_core::ObjectKind::Commit)?;
        let commit = rivet_core::Commit::parse(sub.format(), object.data())?;
        let subject = commit
            .message
            .lines()
            .next()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .unwrap_or_default();
        Ok((commit.committer.when, subject))
    };
    let count;
    match (src, dst) {
        (Side::Gitlink(a), Side::Gitlink(b)) => {
            let bases = rivet_core::merge_bases(sub.odb(), a, b).unwrap_or_default();
            let mut left = options.clone();
            let mut right = options.clone();
            for base in &bases {
                left.exclude(*base);
                right.exclude(*base);
            }
            let left_commits = walk_oids(sub.odb(), &[a], &left)?;
            let right_commits = walk_oids(sub.odb(), &[b], &right)?;
            count = left_commits.len() + right_commits.len();
            for (marker, commits) in [('<', left_commits), ('>', right_commits)] {
                for oid in commits {
                    let (when, subject) = subject_and_date(&oid)?;
                    lines.push((when, marker, subject));
                }
            }
            // Newest first; the right side wins a tie, as it is walked first.
            lines.sort_by_key(|x| std::cmp::Reverse(x.0));
        }
        (Side::Gitlink(only), Side::Other(_) | Side::None)
        | (Side::Other(_) | Side::None, Side::Gitlink(only)) => {
            let deleted = dst == Side::None;
            let walked = walk_oids(sub.odb(), &[only], &options)?;
            count = walked.len();
            if !deleted && let Some(tip) = walked.first() {
                let (when, subject) = subject_and_date(tip)?;
                lines.push((when, '>', subject));
            }
            if deleted {
                text.push_str(":\n\n");
                return Ok(text);
            }
        }
        _ => return Ok(String::new()),
    }
    text.push_str(&format!(" ({count}):\n"));
    let shown_lines = if limit > 0 {
        lines.len().min(limit as usize)
    } else {
        lines.len()
    };
    for (_, marker, subject) in lines.into_iter().take(shown_lines) {
        text.push_str(&format!("  {marker} {subject}\n"));
    }
    text.push('\n');
    Ok(text)
}
