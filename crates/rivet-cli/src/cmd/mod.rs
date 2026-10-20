//! The command handlers: one module per command group, plus the dispatcher that routes a parsed
//! [`crate::cli::Commands`] to one of them.
//!
//! # The shape of a command
//!
//! Every subcommand has a **handler** — a `pub fn run_*` in one of these modules — with a body
//! that is `todo!()` at scaffold time and a doc comment stating three things the implementer
//! needs before writing a line of it:
//!
//! 1. **Purpose** — what the command does, in the user's terms.
//! 2. **Exit codes** — which is load-bearing, because Git's exit codes are part of its
//!    interface and scripts branch on them. `0` success, `1` a requested and expected negative
//!    answer (`diff --quiet` with differences, `check-ignore` with no match, `diff` itself),
//!    `128` a usage or repository error, and — where Git documents one — `2` for the
//!    plumbing commands that distinguish "ran, and the answer is no" from "could not run".
//! 3. **Caveats** — the invariants and Git behaviours that will bite.
//!
//! # Where errors go
//!
//! Every handler returns [`anyhow::Result`]. `rivet-core` has exactly one error type and never
//! panics on repository data; the CLI's job is to attach the context that turns
//! [`rivet_core::Error::NotARepository`] into `fatal: not a git repository (or any of the
//! parent directories)`, and to map the error onto the exit code the handler's doc comment
//! promises. **No handler defines a new error type** — `anyhow`'s opaque error and
//! `rivet_core`'s `Error` are the only two in the binary.
//!
//! # Opening a repository
//!
//! [`open`] and [`open_or_init`] are the two entry points a handler uses, and they exist so
//! that the `--repo` decision is written once. A handler must not call
//! `rivet_core::Repository::open` directly: that reads the current directory, which is wrong
//! whenever `--repo` was given.

pub mod branching;
pub mod history;
pub mod ignore;
pub mod plumbing;
pub mod remote;
pub mod setup;
pub mod snapshot;
pub mod tag;

use std::path::Path;

use anyhow::Result;

use crate::cli::{Cli, Commands, GlobalArgs};
use rivet_core::Repository;

/// The exit code of a command that ran and succeeded.
pub const EXIT_OK: i32 = 0;

/// The exit code of a command that ran, and whose answer is a documented negative.
///
/// # Caveats
///
/// **This is not an error code.** `rivet diff --quiet` on a dirty work tree, `rivet check-ignore`
/// on a path with no matching rule, and `rivet merge-base --is-ancestor` that answers "no" all
/// exit `1` having done exactly what they were asked. A script that treats a non-zero exit as a
/// failure will misreport all three, which is why the three commands document it individually
/// rather than sharing a "failed" helper.
pub const EXIT_FALSE: i32 = 1;

/// The exit code of a command that could not be carried out.
///
/// # Caveats
///
/// **`128` is Git's catch-all for a fatal error** — not a repository, a corrupt object, an
/// unknown revision, a refused push. It is distinct from `129`, which is an *unrecognised
/// command line* and which this binary never returns because `clap` exits on a parse failure
/// before a handler runs.
pub const EXIT_FATAL: i32 = 128;

/// Route a parsed command line to its handler.
///
/// # Caveats
///
/// * **The match is the command list**: every variant in [`crate::cli::Commands`] has an arm, so
///   adding a subcommand breaks the build here instead of silently doing nothing. Each arm is one
///   line; the work lives in the handler it names.
/// * **Global flags are passed to every handler**, not applied here. A handler that needs
///   `--quiet` reads it from its own [`GlobalArgs`] argument, because whether `--quiet`
///   suppresses *this* command's output is a per-command question — `rivet push --quiet` and
///   `rivet log --quiet` suppress different streams.
/// * **The repo is opened per handler, not here.** Opening is fallible and the error is not
///   always fatal in the same way: `rivet init` and `rivet clone` must *not* need a repository,
///   and `rivet config --global` must not either. Opening in the dispatcher would make those
///   three commands impossible to write.
pub fn dispatch(cli: Cli) -> Result<i32> {
    let Cli { global, command } = cli;
    match command {
        Commands::Init(args) => setup::run_init(&global, &args),
        Commands::Clone(args) => setup::run_clone(&global, &args),
        Commands::Config(args) => setup::run_config(&global, &args),
        Commands::Add(args) => snapshot::run_add(&global, &args),
        Commands::Rm(args) => snapshot::run_rm(&global, &args),
        Commands::Commit(args) => snapshot::run_commit(&global, &args),
        Commands::Status(args) => snapshot::run_status(&global, &args),
        Commands::Restore(args) => snapshot::run_restore(&global, &args),
        Commands::Reset(args) => snapshot::run_reset(&global, &args),
        Commands::Log(args) => history::run_log(&global, &args),
        Commands::Show(args) => history::run_show(&global, &args),
        Commands::Diff(args) => history::run_diff(&global, &args),
        Commands::Branch(args) => branching::run_branch(&global, &args),
        Commands::Switch(args) => branching::run_switch(&global, &args),
        Commands::Checkout(args) => branching::run_checkout(&global, &args),
        Commands::Merge(args) => branching::run_merge(&global, &args),
        Commands::Tag(args) => tag::run_tag(&global, &args),
        Commands::CheckIgnore(args) => ignore::run_check_ignore(&global, &args),
        Commands::Remote(args) => remote::run_remote(&global, &args),
        Commands::Fetch(args) => remote::run_fetch(&global, &args),
        Commands::Pull(args) => remote::run_pull(&global, &args),
        Commands::Push(args) => remote::run_push(&global, &args),
        Commands::HashObject(args) => plumbing::run_hash_object(&global, &args),
        Commands::CatFile(args) => plumbing::run_cat_file(&global, &args),
        Commands::LsTree(args) => plumbing::run_ls_tree(&global, &args),
        Commands::LsFiles(args) => plumbing::run_ls_files(&global, &args),
        Commands::RevParse(args) => plumbing::run_rev_parse(&global, &args),
        Commands::WriteTree(args) => plumbing::run_write_tree(&global, &args),
        Commands::VerifyPack(args) => plumbing::run_verify_pack(&global, &args),
        Commands::Fsck(args) => plumbing::run_fsck(&global, &args),
    }
}

/// Open the repository the global flags point at.
///
/// # Caveats
///
/// * **`--repo` starts discovery; it does not name a git directory.** `rivet --repo /srv/x
///   status` walks up from `/srv/x` exactly as if the process had been started there, which is
///   what makes it usable from a subdirectory of a work tree. A caller that wanted to name the
///   git directory itself is asking for `GIT_DIR`, which the environment already provides and
///   which this function deliberately does not synthesise.
/// * **`--repo` does not change the process's working directory**, and must not: relative paths
///   that appear in the *output* (`status`, `diff --name-only`) are relative to where the user
///   is standing, and a `chdir` would silently rewrite them.
/// * **The environment is consulted**, so `GIT_DIR` and `GIT_WORK_TREE` still override
///   everything. That ordering is Git's and is why this lowers to
///   [`rivet_core::Repository::open_or_discover`] rather than to `discover`.
///
/// # Errors
///
/// [`rivet_core::Error::NotARepository`], wrapped with the text Git prints, when neither
/// `--repo` nor the current directory is inside a repository.
pub fn open(global: &GlobalArgs) -> Result<Repository> {
    let start = match &global.repo {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    match Repository::open_or_discover(&start) {
        Ok(repository) => Ok(repository),
        // Git's message names `.git` and says "parent" rather than "ancestor"; the differential
        // tests compare the text, so the wording is quoted from `git status` outside a repository.
        Err(rivet_core::Error::NotARepository(_)) => Err(anyhow::anyhow!(
            "not a git repository (or any of the parent directories): .git"
        )),
        Err(error) => Err(error.into()),
    }
}

/// Open the repository the global flags point at, or create one if there is none.
///
/// # Caveats
///
/// * **Used only by `rivet init` and `rivet clone`**, and it is the *absence* of a repository
///   that it turns into a creation rather than an error. Every other command uses [`open`].
/// * **A `--repo` path that does not exist is not an error here** — it is a directory to be
///   created, which is what `rivet init --repo /new/place` means.
/// * **"There is no repository" is tested as [`rivet_core::Error::NotARepository`] and nothing
///   else.** A corrupt config or an unreadable parent directory must propagate as the error it
///   is; swallowing it and initialising a fresh repository on top would destroy the user's
///   `.git` without ever printing why.
///
/// # Errors
///
/// As [`rivet_core::Repository::init`], plus any error other than `NotARepository` that
/// [`open`] raised.
pub fn open_or_init(global: &GlobalArgs) -> Result<Repository> {
    let directory = base_dir(global)?;
    match Repository::open_or_discover(&directory) {
        Ok(repository) => Ok(repository),
        Err(rivet_core::Error::NotARepository(_)) => {
            let options = rivet_core::InitOptions {
                bare: false,
                initial_branch: rivet_core::ops::default_branch_name()?,
                format: rivet_core::ObjectFormat::Sha1,
                work_tree: None,
            };
            Repository::init(&directory, &options).map_err(Into::into)
        }
        Err(error) => Err(error.into()),
    }
}

/// The directory a path-valued global flag names, or the current directory.
///
/// # Caveats
///
/// **The fallback is the process's working directory read at call time**, which is why this is
/// a function rather than a field: a handler that cached it before an operation would validate
/// paths against a directory the user had since left. Returns an owned `PathBuf` rather than a
/// `&Path` because `--repo` is owned by the argument struct and the current directory has to be
/// materialised anyway.
pub fn base_dir(global: &GlobalArgs) -> Result<std::path::PathBuf> {
    match &global.repo {
        Some(path) => Ok(path.clone()),
        None => Ok(std::env::current_dir()?),
    }
}

/// Turn a command-line path into the byte string `rivet-core` expects.
///
/// # Caveats
///
/// * **This is the one place the CLI's text meets the library's bytes**, and the conversion is
///   lossless on Unix because it goes through `OsStrExt` rather than through `to_str()`. A
///   `to_str().unwrap()` here would make every non-UTF-8 filename unreachable from the command
///   line, which is precisely the class of file Git is careful to support.
/// * **This function only converts; it does not normalise.** Resolving `.`/`..` against the
///   work tree and refusing paths that escape it lives in [`worktree_path`], which needs the
///   repository to know where the work tree is. Callers that take a path from the command line
///   should call that one.
/// * **On Windows the separators are rewritten to `/`**, because a Git tree entry's name never
///   contains a backslash and would otherwise produce a tree that `git fsck` rejects.
pub fn path_arg(path: &Path) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        let text = path
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        Ok(text.into_owned().into_bytes())
    }
}

/// Resolve a command-line path to the work-tree-relative bytes `rivet-core` expects.
///
/// Relative paths are joined to the process's current directory, `.` and `..` are resolved
/// lexically, and a path that lands outside the work tree is refused here rather than being
/// handed to the library as a `..`-prefixed tree entry. `op` names the command so the error
/// reads like Git's ("add: '...' is outside the work tree '...'").
pub fn worktree_path(repository: &Repository, path: &Path, op: &str) -> Result<bstr::BString> {
    let Some(root) = repository.worktree()?.root().map(Path::to_path_buf) else {
        anyhow::bail!("{op}: this operation must be run in a work tree");
    };

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let mut cleaned = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                cleaned.pop();
            }
            other => cleaned.push(other.as_os_str()),
        }
    }

    let relative = cleaned.strip_prefix(&root).map_err(|_| {
        anyhow::anyhow!(
            "{op}: '{}' is outside the work tree '{}'",
            path.display(),
            root.display()
        )
    })?;

    if relative.as_os_str().is_empty() {
        return Ok(bstr::BString::from("."));
    }
    Ok(path_arg(relative)?.into())
}

/// The seven-hex-digit abbreviation Git prints by default.
pub fn short_oid(oid: &rivet_core::ObjectId) -> String {
    oid.to_string().chars().take(7).collect()
}

/// Resolve a revision to the object it names, without peeling.
pub fn resolve_rev(repository: &Repository, spec: &str) -> Result<rivet_core::ObjectId> {
    rivet_core::revparse::resolve(
        repository.format(),
        repository.refs(),
        repository.odb(),
        bstr::BStr::new(spec.as_bytes()),
    )
    .map_err(|_| anyhow::anyhow!("not a valid object name: '{spec}'"))
}

/// Resolve a revision and peel it to a commit, with Git's wording on failure.
pub fn resolve_commit(repository: &Repository, spec: &str) -> Result<rivet_core::ObjectId> {
    let oid = resolve_rev(repository, spec)?;
    repository
        .refs()
        .peel_oid(oid, repository.odb())
        .map_err(|_| anyhow::anyhow!("Not a valid commit name {spec}"))
}

/// The first line of a commit's message, as `--oneline` and the switch/reset messages print it.
pub fn commit_subject(repository: &Repository, oid: &rivet_core::ObjectId) -> Result<String> {
    let object = repository.odb().read(oid)?;
    match object.parse()? {
        rivet_core::AnyObject::Commit(commit) => {
            let message = commit.message;
            let first = message
                .split(|b| *b == b'\n')
                .find(|line| !line.iter().all(u8::is_ascii_whitespace))
                .unwrap_or(&[]);
            Ok(String::from_utf8_lossy(first).trim_end().to_string())
        }
        _ => Ok(String::new()),
    }
}

/// How the reflog names where `HEAD` was: the branch's short name, or the full id when detached.
pub fn head_reflog_label(repository: &Repository) -> Result<String> {
    Ok(match repository.head()? {
        rivet_core::HeadState::Attached { branch, .. } | rivet_core::HeadState::Unborn { branch } => {
            branch.short_name().to_string()
        }
        rivet_core::HeadState::Detached { oid } => oid.to_string(),
    })
}

/// Print the diffstat and the `create mode`/`delete mode` summary `merge` and `pull` show
/// after updating a branch.
pub fn print_diffstat(
    repository: &Repository,
    old_tree: Option<rivet_core::ObjectId>,
    new_tree: rivet_core::ObjectId,
) -> Result<()> {
    let old_tree = match old_tree {
        Some(tree) => tree,
        None => empty_tree(repository)?,
    };
    let options = rivet_core::DiffOptions::new();
    let diff = rivet_core::tree_diff(repository.format(), old_tree, new_tree, repository.odb(), &options)?;
    let mut out = Vec::new();
    rivet_core::format_stat_sized(&diff, Some(repository.odb()), &mut out)?;
    print_summary(&diff, &mut out);
    use std::io::Write as _;
    std::io::stdout().write_all(&out)?;
    Ok(())
}

/// Git's `--summary` lines: creations, deletions and mode changes.
pub fn print_summary(diff: &rivet_core::TreeDiff, out: &mut Vec<u8>) {
    for file in &diff.files {
        let path = file.path();
        if file.is_new {
            let mode = file.new_mode.map(|m| m.to_u32()).unwrap_or(0);
            out.extend_from_slice(format!(" create mode {mode:06o} {path}\n").as_bytes());
        } else if file.is_deleted {
            let mode = file.old_mode.map(|m| m.to_u32()).unwrap_or(0);
            out.extend_from_slice(format!(" delete mode {mode:06o} {path}\n").as_bytes());
        } else if let (Some(old), Some(new)) = (file.old_mode, file.new_mode) {
            if old != new && old.to_u32() & 0o170000 == new.to_u32() & 0o170000 {
                out.extend_from_slice(
                    format!(" mode change {:06o} => {:06o} {path}\n", old.to_u32(), new.to_u32()).as_bytes(),
                );
            }
        }
    }
}

/// The one-line shortstat plus the `create mode` summary, as `git commit` prints them.
pub fn print_shortstat_summary(
    repository: &Repository,
    old_tree: Option<rivet_core::ObjectId>,
    new_tree: rivet_core::ObjectId,
) -> Result<()> {
    let old_tree = match old_tree {
        Some(tree) => tree,
        None => empty_tree(repository)?,
    };
    let options = rivet_core::DiffOptions::new();
    let diff = rivet_core::tree_diff(repository.format(), old_tree, new_tree, repository.odb(), &options)?;
    let mut stat = Vec::new();
    rivet_core::format_stat_sized(&diff, Some(repository.odb()), &mut stat)?;
    let text = String::from_utf8_lossy(&stat);
    let mut out = Vec::new();
    if let Some(last) = text.lines().last() {
        out.extend_from_slice(last.as_bytes());
        out.push(b'\n');
    }
    print_summary(&diff, &mut out);
    use std::io::Write as _;
    std::io::stdout().write_all(&out)?;
    Ok(())
}

/// The id of the empty tree, which the diff machinery treats as "no entries" without it being
/// stored — writing it would leave a dangling object for `fsck` to report.
pub fn empty_tree(repository: &Repository) -> Result<rivet_core::ObjectId> {
    Ok(rivet_core::hash_object(repository.format(), rivet_core::ObjectKind::Tree, b"")?.0)
}
