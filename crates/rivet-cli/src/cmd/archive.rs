//! `archive`: write a tree as a tar, tar.gz or zip archive.
//!
//! The option grammar is Git's own: `--prefix` applies to what follows it (so it also decides the
//! path of a later `--add-file`), options and paths may be mixed after the tree-ish, and `-0`…`-9`
//! set the compression level. `--remote` works for local repositories only (the archive is made
//! in place, without a daemon), `--exec` only as `git-upload-archive`, `--worktree-attributes` reads
//! `.gitattributes` from the work tree. Refused by name: `--add-virtual-file`, network remotes.

use std::io::Write;

use anyhow::Result;
use bstr::BString;
use rivet_core::archive::{ArchiveFile, ArchiveFormat, ArchiveOptions, archive};

use crate::cli::GlobalArgs;

/// Arguments of `archive`: everything after the command, in Git's own grammar.
#[derive(Debug, clap::Args)]
pub struct ArchiveArgs {
    /// `[<options>] <tree-ish> [<path>...]`.
    #[arg(value_name = "ARGS", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

const USAGE: &str = "usage: git archive [<options>] <tree-ish> [<path>...]
   or: git archive --list
   or: git archive --remote <repo> [--exec <cmd>] [<options>] <tree-ish> [<path>...]
   or: git archive --remote <repo> [--exec <cmd>] --list

    --[no-]format <fmt>   archive format
    --[no-]prefix <prefix>
                          prepend prefix to each pathname in the archive
    --[no-]add-file <file>
                          add untracked file to archive
    --[no-]add-virtual-file <path:content>
                          add untracked file to archive
    -o, --[no-]output <file>
                          write the archive to this file
    --[no-]worktree-attributes
                          read .gitattributes in working directory
    -v, --[no-]verbose    report archived files on stderr
    --mtime <time>        set modification time of archive entries
    -NUM                  set compression level

    -l, --[no-]list       list supported archive formats

    --[no-]remote <repo>  retrieve the archive from remote repository <repo>
    --[no-]exec <command> path to the remote git-upload-archive command

";

fn format_by_name(name: &str) -> Option<ArchiveFormat> {
    match name {
        "tar" => Some(ArchiveFormat::Tar),
        "tar.gz" | "tgz" => Some(ArchiveFormat::TarGz),
        "zip" => Some(ArchiveFormat::Zip),
        _ => None,
    }
}

fn format_by_filename(name: &str) -> Option<ArchiveFormat> {
    if name.ends_with(".tar") {
        Some(ArchiveFormat::Tar)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Some(ArchiveFormat::TarGz)
    } else if name.ends_with(".zip") {
        Some(ArchiveFormat::Zip)
    } else {
        None
    }
}

/// A revision, or `<revision>:<path>`.
fn resolve(repository: &rivet_core::Repository, spec: &str) -> Option<rivet_core::ObjectId> {
    if spec.contains(':') {
        rivet_core::revparse::resolve_path(
            repository.format(),
            repository.refs(),
            repository.odb(),
            bstr::BStr::new(spec.as_bytes()),
        )
        .ok()
    } else {
        super::resolve_rev(repository, spec).ok()
    }
}

/// Handler for `rivet archive`.
pub fn run_archive(global: &GlobalArgs, args: &ArchiveArgs) -> Result<i32> {
    let mut format_name: Option<String> = None;
    let mut output: Option<String> = None;
    let mut prefix = String::new();
    let mut level: Option<u32> = None;
    let mut mtime: Option<i64> = None;
    let mut list = false;
    let mut verbose = global.verbose;
    let mut extra: Vec<ArchiveFile> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut remote: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut worktree_attributes = false;

    let mut iter = args.args.iter();
    let mut only_paths = false;
    while let Some(arg) = iter.next() {
        if only_paths {
            positional.push(arg.clone());
            continue;
        }
        // `--opt=value` and `--opt value` are the same thing; `value_of` takes the next word.
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value_of = |what: &str| -> Option<String> {
            inline.clone().or_else(|| iter.next().cloned()).or_else(|| {
                eprintln!("error: option `{what}' requires a value");
                None
            })
        };
        match name {
            "--" => only_paths = true,
            "-l" | "--list" => list = true,
            "-v" | "--verbose" => verbose = true,
            "--format" => match value_of("format") {
                Some(v) => format_name = Some(v),
                None => {
                    eprint!("{USAGE}");
                    return Ok(129);
                }
            },
            "--prefix" => match value_of("prefix") {
                Some(v) => prefix = v,
                None => {
                    eprint!("{USAGE}");
                    return Ok(129);
                }
            },
            "-o" | "--output" => match value_of("output") {
                Some(v) => output = Some(v),
                None => {
                    eprint!("{USAGE}");
                    return Ok(129);
                }
            },
            "--mtime" => match value_of("mtime") {
                Some(v) => match v.trim_start_matches('@').parse::<i64>() {
                    Ok(t) => mtime = Some(t),
                    Err(_) => {
                        anyhow::bail!("archive: --mtime {v} is not implemented (use seconds)")
                    }
                },
                None => {
                    eprint!("{USAGE}");
                    return Ok(129);
                }
            },
            "--add-file" => {
                let Some(file) = value_of("add-file") else {
                    eprint!("{USAGE}");
                    return Ok(129);
                };
                let data = match std::fs::read(&file) {
                    Ok(data) => data,
                    Err(error) => {
                        let _ = error;
                        eprintln!("fatal: File not found: {file}");
                        return Ok(128);
                    }
                };
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::metadata(&file)
                        .map(|m| m.permissions().mode() & 0o100 != 0)
                        .unwrap_or(false)
                };
                #[cfg(not(unix))]
                let exec = false;
                let base = std::path::Path::new(&file)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                extra.push(ArchiveFile {
                    path: BString::from(format!("{prefix}{base}")),
                    mode: if exec { 0o100755 } else { 0o100644 },
                    data,
                });
            }
            "--remote" | "--exec" => {
                let Some(v) = value_of(&name[2..]) else {
                    eprint!("{USAGE}");
                    return Ok(129);
                };
                if name == "--remote" {
                    remote = Some(v);
                } else {
                    exec = Some(v);
                }
            }
            "--worktree-attributes" => worktree_attributes = true,
            "--no-worktree-attributes" => worktree_attributes = false,
            "--add-virtual-file" => {
                anyhow::bail!("archive: {name} is not implemented")
            }
            a if a.len() == 2 && a.starts_with('-') && a.as_bytes()[1].is_ascii_digit() => {
                level = Some(u32::from(a.as_bytes()[1] - b'0'));
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("error: unknown option `{}'", a.trim_start_matches('-'));
                eprint!("{USAGE}");
                return Ok(129);
            }
            _ => positional.push(arg.clone()),
        }
    }

    if list {
        println!("tar\ntgz\ntar.gz\nzip");
        return Ok(0);
    }
    let Some((treeish, paths)) = positional.split_first() else {
        eprint!("{USAGE}");
        return Ok(129);
    };
    let format = match &format_name {
        Some(name) => match format_by_name(name) {
            Some(f) => f,
            None => {
                eprintln!("fatal: Unknown archive format '{name}'");
                return Ok(128);
            }
        },
        None => output
            .as_deref()
            .and_then(format_by_filename)
            .unwrap_or(ArchiveFormat::Tar),
    };

    let repository = if let Some(remote) = &remote {
        if let Some(command) = exec.as_deref().filter(|e| *e != "git-upload-archive") {
            // The command is run through the shell with the repository as its argument. A command
            // that does not exist fails the way git's does; any other would need the wire
            // protocol, which is not spoken here.
            let script = format!("{command} '{remote}'");
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .arg(&script)
                .stdin(std::process::Stdio::null())
                .output()?;
            if output.status.code() == Some(127) {
                eprint!("{}", String::from_utf8_lossy(&output.stderr));
                eprintln!("fatal: the remote end hung up unexpectedly");
                return Ok(128);
            }
            anyhow::bail!(
                "archive: --exec with a command other than git-upload-archive is not implemented"
            );
        }
        if !extra.is_empty() || worktree_attributes {
            anyhow::bail!(
                "archive: --remote with --add-file or --worktree-attributes is not implemented"
            );
        }
        let path = remote.strip_prefix("file://").unwrap_or(remote);
        if path.contains("://") || (path.contains(':') && !path.contains('/')) {
            anyhow::bail!(
                "archive: --remote with a network url is not implemented (local paths only)"
            );
        }
        // What `git upload-archive` answers over the wire, minus the wire: the repository is
        // opened in place and its own refs, attributes and objects decide the archive.
        // The output file exists as soon as the request is made, whatever the server answers.
        if let Some(out) = &output {
            std::fs::write(out, b"")?;
        }
        let candidate = std::path::Path::new(path);
        let repository = (candidate.join(".git").exists()
            || (candidate.join("HEAD").exists() && candidate.join("objects").exists()))
        .then(|| rivet_core::Repository::discover(candidate).ok())
        .flatten();
        let Some(repository) = repository else {
            return Ok(remote_died(&format!(
                "fatal: '{remote}' does not appear to be a git repository"
            )));
        };
        // The server only takes names of refs (optionally followed by `:<path>`), not expressions.
        let name = treeish.split(':').next().unwrap_or_default();
        let named = name == "HEAD" || super::query::dwim_ref_name(&repository, name).is_some();
        let oid = named.then(|| resolve(&repository, treeish)).flatten();
        let Some(oid) = oid else {
            return Ok(remote_died(&format!("fatal: no such ref: {treeish}")));
        };
        return write_archive(
            &repository,
            oid,
            format,
            &prefix,
            String::new(),
            level,
            mtime,
            paths,
            Vec::new(),
            verbose,
            false,
            output,
            true,
        );
    } else {
        super::open(global)?
    };
    let oid = match resolve(&repository, treeish) {
        Some(oid) => oid,
        None => {
            eprintln!("fatal: not a valid object name: {treeish}");
            return Ok(128);
        }
    };
    // Run from a subdirectory, Git archives that subdirectory.
    let mut base = String::new();
    if let (Some(root), Ok(cwd)) = (repository.worktree_path(), super::base_dir(global))
        && let Ok(rel) = std::path::absolute(&cwd)?.strip_prefix(root)
    {
        base = rel.to_string_lossy().into_owned();
    }
    write_archive(
        &repository,
        oid,
        format,
        &prefix,
        base,
        level,
        mtime,
        paths,
        extra,
        verbose,
        worktree_attributes,
        output,
        false,
    )
}

/// What a remote `git upload-archive` sends back when it dies, as the client prints it.
fn remote_died(message: &str) -> i32 {
    eprintln!("remote: {message}        ");
    eprintln!("remote: git upload-archive: archiver died with error");
    eprintln!("fatal: sent error to the client: git upload-archive: archiver died with error");
    1
}

#[allow(clippy::too_many_arguments)]
fn write_archive(
    repository: &rivet_core::Repository,
    oid: rivet_core::ObjectId,
    format: ArchiveFormat,
    prefix: &str,
    base: String,
    level: Option<u32>,
    mtime: Option<i64>,
    paths: &[String],
    extra: Vec<ArchiveFile>,
    verbose: bool,
    worktree_attributes: bool,
    output: Option<String>,
    remote: bool,
) -> Result<i32> {
    let mut options = ArchiveOptions::new(format);
    options.prefix = BString::from(prefix);
    options.base = BString::from(base);
    options.worktree_attributes = worktree_attributes;
    options.level = level;
    options.time = mtime;
    options.paths = paths.iter().map(|p| BString::from(p.as_str())).collect();
    options.extra_files = extra;

    options.verbose = verbose;

    let mut bytes: Vec<u8> = Vec::new();
    if let Err(error) = archive(repository, oid, &options, &mut bytes) {
        if remote {
            return Ok(remote_died(&format!("fatal: {error}")));
        }
        eprintln!("fatal: {error}");
        return Ok(128);
    }
    match output {
        Some(path) => std::fs::write(&path, &bytes)?,
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&bytes)?;
            stdout.flush()?;
        }
    }
    Ok(0)
}
