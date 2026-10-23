//! `submodule`: `status` only, and `.gitmodules` read-only.
//!
//! Each gitlink of the index prints as `<state><commit> <path>[ (<description>)]`, where the state
//! is `-` for a submodule that is not checked out, `+` when its `HEAD` is not the recorded commit,
//! `U` for a merge conflict and a blank otherwise. Every other subcommand (`init`, `update`, `add`,
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
                anyhow::bail!("submodule: '{other}' is not implemented (only 'status' is)")
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

/// The `path` of every `[submodule "<name>"]` in `.gitmodules`, with the submodule's name. Git
/// reads the file from the work tree and, when it is not there, from the index.
fn mappings(repository: &Repository, root: &Path) -> Vec<(BString, BString)> {
    let from_blob = |oid: &ObjectId| {
        let blob = repository
            .odb()
            .read_typed(oid, rivet_core::ObjectKind::Blob)
            .ok()?;
        rivet_core::Config::parse(blob.data()).ok()
    };
    let config = match std::fs::read(root.join(".gitmodules")) {
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
    };
    let Some(config) = config else {
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
