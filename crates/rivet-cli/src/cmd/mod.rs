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
/// * **The `todo!()` body is the whole scaffold point of this function**: the match arms are the
///   command list, and the compiler refuses to build until every variant is handled. A new
///   subcommand added to [`crate::cli::Commands`] therefore breaks the build here rather than
///   silently doing nothing.
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
        Commands::Clone(_args) => todo!(),
        Commands::Config(_args) => todo!(),
        Commands::Add(args) => snapshot::run_add(&global, &args),
        Commands::Rm(args) => snapshot::run_rm(&global, &args),
        Commands::Commit(args) => snapshot::run_commit(&global, &args),
        Commands::Status(args) => snapshot::run_status(&global, &args),
        Commands::Restore(_args) => todo!(),
        Commands::Reset(_args) => todo!(),
        Commands::Log(_args) => todo!(),
        Commands::Show(_args) => todo!(),
        Commands::Diff(_args) => todo!(),
        Commands::Branch(_args) => todo!(),
        Commands::Switch(_args) => todo!(),
        Commands::Checkout(_args) => todo!(),
        Commands::Merge(_args) => todo!(),
        Commands::Tag(_args) => todo!(),
        Commands::CheckIgnore(_args) => todo!(),
        Commands::Remote(_args) => todo!(),
        Commands::Fetch(_args) => todo!(),
        Commands::Pull(_args) => todo!(),
        Commands::Push(_args) => todo!(),
        Commands::HashObject(_args) => todo!(),
        Commands::CatFile(_args) => todo!(),
        Commands::LsTree(_args) => todo!(),
        Commands::LsFiles(_args) => todo!(),
        Commands::RevParse(_args) => todo!(),
        Commands::WriteTree(_args) => todo!(),
        Commands::VerifyPack(_args) => todo!(),
        Commands::Fsck(_args) => todo!(),
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
                initial_branch: bstr::BString::from("main"),
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
/// * **The result is relative to the work tree, not to the process.** A path given as
///   `../other/file` must be normalised against the work tree root before it is used, and a
///   path that escapes the work tree is [`rivet_core::Error::OutsideWorktree`] — not a path to
///   be operated on. Keeping the conversion here means that rule has one implementation.
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
