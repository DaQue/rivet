//! The `rivet` command-line grammar: the parser, the subcommand enum, the global flags, and the
//! small value enums (`--object-format=sha256`, `--untracked-files=all`, …) that the argument
//! structs share.
//!
//! # Layout
//!
//! * This module owns the **grammar** — [`Cli`], [`GlobalArgs`], [`Commands`] and the shared
//!   value enums below.
//! * Each command group's **arguments** live next to its handlers in [`crate::cmd`]: the
//!   `snapshot` module owns `AddArgs`, `CommitArgs` and the rest, and so on. Adding a flag means
//!   touching exactly one file.
//! * [`crate::cmd::dispatch`] is the single function that routes a parsed [`Commands`] to its
//!   handler.
//!
//! # Caveats
//!
//! * **These argument types are deliberately not `rivet-core` types.** `clap` parses UTF-8
//!   `String`s and `PathBuf`s; `rivet-core` speaks `bstr::BString`. The conversion happens in
//!   the handler, at the one place where the CLI's text meets the library's bytes. A field
//!   typed `BString` here would not compile at all: `bstr` is neither a `clap` value parser nor
//!   a dependency of this crate.
//! * **Paths are `PathBuf`, never `String`.** A Git path is bytes and need not be UTF-8; a
//!   `PathBuf` carries it through `OsString` intact, where a `String` field would make every
//!   file whose name is not valid UTF-8 unreachable from the command line.
//! * **There is exactly one global `-v` and one global `-q`.** Subcommands must not redeclare
//!   them; they inherit. Where Git gives `-v` a *different* meaning inside a subcommand —
//!   `git tag -v` is `--verify` — that flag is declared long-only in this grammar, and its doc
//!   comment says so.
//! * **`--repo` has no short form.** Git spells the equivalent `git -C <path>`, which is only
//!   accepted *before* the subcommand. A global `-C` here would collide with `commit -C` and
//!   `merge -C` (`--reuse-message`), both of which are in this grammar and both of which a
//!   script is more likely to depend on.
//! * **Every field is `pub` and every handler takes `&Args`.** Handlers never re-parse
//!   `std::env::args()`; the parsed values are the only source of truth, which is what makes
//!   the handlers testable with a constructed `Args` rather than a subprocess.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::cmd::{
    branching, clean, history, ignore, mv, plumbing, reflog, remote, sequencer, setup, snapshot,
    stash, tag,
};

/// The `rivet` command line.
///
/// # Caveats
///
/// * **A subcommand is required.** `rivet` on its own prints the command list and exits
///   non-zero rather than guessing at a default; see `arg_required_else_help` below.
/// * **The global flags are `global = true`**, so `rivet --quiet status` and
///   `rivet status --quiet` are the same invocation. That is deliberate: it is what makes a
///   wrapper script able to append `--quiet` to a command it did not construct.
#[derive(Debug, Parser)]
#[command(
    name = "rivet",
    version,
    about = "A Git-compatible version control system",
    long_about = "Rivet reads and writes real Git repositories: it uses `.git/`, Git's object \
                  format, Git's index and Git's refs, so `git` and GitHub can work on the same \
                  repository that Rivet is working on.",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Flags that apply to every subcommand.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Commands,
}

/// Flags accepted before or after any subcommand.
///
/// # Caveats
///
/// * **`repo` is a change of directory, not a hint.** When it is set, discovery starts there and
///   the process's own working directory is never consulted; that is what makes
///   `rivet --repo /srv/dotfiles status` work from anywhere. It is *not* `GIT_DIR`: setting
///   `GIT_DIR` in the environment still overrides discovery entirely, and `--repo` only decides
///   where discovery begins. See `rivet_core::repo::GIT_DIR_ENV`.
/// * **`verbose` and `quiet` are not mutually exclusive here.** Git lets the last one win;
///   enforcing that needs `overrides_with`, and a subcommand that redeclares either flag would
///   then break. The handlers read `quiet` first and treat `verbose` as the fallback, so
///   `--quiet --verbose` is quiet. The precedence is deliberately explicit.
/// * **`no_pager` is currently the only pager-related flag, and there is no pager yet.** Output
///   is written straight to stdout; the flag exists so that a script written against this
///   grammar keeps working once `rivet_core::Repository` grows one.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Operate on the repository at PATH instead of discovering one from the current directory.
    #[arg(long = "repo", global = true, value_name = "PATH")]
    pub repo: Option<PathBuf>,

    /// Report progress and per-object diagnostics on stderr.
    #[arg(short = 'v', long = "verbose", global = true)]
    pub verbose: bool,

    /// Suppress everything that is not the command's own result.
    #[arg(short = 'q', long = "quiet", global = true)]
    pub quiet: bool,

    /// Never start a pager, even when stdout is a terminal.
    #[arg(long = "no-pager", global = true)]
    pub no_pager: bool,
}

/// Every `rivet` subcommand.
///
/// # Caveats
///
/// * **The variant name is the command name, in kebab-case.** `HashObject` is
///   `rivet hash-object`, and clap derives that. The names are not all Git's — `rivet restore`
///   and `rivet reset` both exist, and they are *not* aliases for one another, exactly as in
///   Git.
/// * **Argument structs are grouped by area, not by name.** The module a variant's arguments
///   live in says which handler file will implement it; see [`crate::cmd`]. A new subcommand in
///   an existing area is a variant here plus a handler there, and nothing else.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Create an empty repository, or re-initialise an existing one.
    Init(setup::InitArgs),

    /// Clone a repository into a new directory.
    Clone(setup::CloneArgs),

    /// Read and write repository configuration.
    Config(setup::ConfigArgs),

    /// Add file contents to the index.
    Add(snapshot::AddArgs),

    /// Remove files from the work tree and the index.
    Rm(snapshot::RmArgs),

    /// Record changes to the repository.
    Commit(snapshot::CommitArgs),

    /// Show the working tree status.
    Status(snapshot::StatusArgs),

    /// Restore work tree files from the index or a commit.
    Restore(snapshot::RestoreArgs),

    /// Reset the current branch to a commit, and optionally the index and work tree.
    Reset(snapshot::ResetArgs),

    /// Show commit logs.
    Log(history::LogArgs),

    /// Show the objects a commit, tag or tree names.
    Show(history::ShowArgs),

    /// Move or rename a file, a directory or a symlink.
    Mv(mv::MvArgs),

    /// Remove untracked files from the work tree.
    Clean(clean::CleanArgs),

    /// Shelve and restore local changes.
    Stash(stash::StashArgs),

    /// Show a ref's reflog.
    Reflog(reflog::ReflogArgs),

    /// Show changes between the work tree, the index and commits.
    Diff(history::DiffArgs),

    /// List, create, rename and delete branches.
    Branch(branching::BranchArgs),

    /// Switch branches.
    Switch(branching::SwitchArgs),

    /// Check out a branch, tag or commit, or restore paths from one.
    Checkout(branching::CheckoutArgs),

    /// Join development histories together.
    Merge(branching::MergeArgs),

    /// Apply the changes introduced by existing commits.
    #[command(name = "cherry-pick")]
    CherryPick(sequencer::CherryPickArgs),

    /// Record new commits that undo existing ones.
    Revert(sequencer::RevertArgs),

    /// List, create, verify and delete tags.
    Tag(tag::TagArgs),

    /// Debug `.gitignore` rules against paths.
    #[command(name = "check-ignore")]
    CheckIgnore(ignore::CheckIgnoreArgs),

    /// Manage the set of repositories whose branches are tracked.
    Remote(remote::RemoteArgs),

    /// Download objects and refs from another repository.
    Fetch(remote::FetchArgs),

    /// Fetch from a remote and merge into the current branch.
    Pull(remote::PullArgs),

    /// Update remote refs along with the objects they need.
    Push(remote::PushArgs),

    /// Compute an object id and optionally write the object.
    #[command(name = "hash-object")]
    HashObject(plumbing::HashObjectArgs),

    /// Show the type, size or contents of a repository object.
    #[command(name = "cat-file")]
    CatFile(plumbing::CatFileArgs),

    /// List the contents of a tree object.
    #[command(name = "ls-tree")]
    LsTree(plumbing::LsTreeArgs),

    /// List the files recorded in the index.
    #[command(name = "ls-files")]
    LsFiles(plumbing::LsFilesArgs),

    /// Resolve a revision or a repository property to an id or a path.
    #[command(name = "rev-parse")]
    RevParse(plumbing::RevParseArgs),

    /// Write the index's contents out as a tree object.
    #[command(name = "write-tree")]
    WriteTree(plumbing::WriteTreeArgs),

    /// Validate a packfile and its index.
    #[command(name = "verify-pack")]
    VerifyPack(plumbing::VerifyPackArgs),

    /// Verify the connectivity and validity of the objects in the repository.
    Fsck(plumbing::FsckArgs),
}

/// The object format a repository is created with, or expects.
///
/// # Caveats
///
/// **This is a creation-time decision that cannot be reversed.** A repository's object format is
/// fixed when it is initialised and converting means rewriting every object; Git does not do it
/// and neither does Rivet. SHA-1 is the default because GitHub accepts only SHA-1 publicly,
/// while SHA-256 is supported end to end and hosted by GitLab, Codeberg, Forgejo and Gitea. The
/// two cannot exchange history, and Rivet refuses a fetch or push across the boundary with
/// [`rivet_core::Error::ObjectFormatMismatch`] rather than writing objects the far side cannot
/// read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// SHA-1 — Git's default, and the only format GitHub accepts publicly.
    Sha1,

    /// SHA-256 — larger ids, no known practical collisions.
    Sha256,
}

/// How much of the work tree's untracked state `rivet status` reports.
///
/// # Caveats
///
/// **This changes the shape of the walk, not just the output.** `No` skips the untracked scan
/// entirely and is what a script polling for staged changes should use; `Normal` still does not
/// descend into an untracked directory it reports whole; `All` walks every untracked directory
/// file by file, which on a work tree with a build directory is the difference between
/// milliseconds and minutes. See [`rivet_core::UntrackedMode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum UntrackedModeArg {
    /// Do not show untracked files at all.
    No,

    /// Show untracked files and directories, but do not descend into untracked directories.
    Normal,

    /// Show every untracked file individually, descending into every directory.
    All,
}

/// The line-diff algorithm to use.
///
/// # Caveats
///
/// **The two algorithms produce different, equally valid hunks.** Myers is the classic
/// O(ND) algorithm and is what Git has always used; histogram (patience-style) often produces a
/// more readable diff on files with repeated blocks, and which one produced a hunk is recorded
/// on `rivet_core::diff::FileDiff::algorithm` so a caller can tell. A test comparing Rivet's
/// output to `git diff` byte for byte must pin the same algorithm on both sides. See
/// [`rivet_core::DiffAlgorithm`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum DiffAlgorithmArg {
    /// Myers' O(ND) algorithm — Git's default.
    Myers,

    /// Histogram diff, which handles repeated blocks more readably.
    Histogram,
}

/// The kind of object a plumbing command reads or writes.
///
/// # Caveats
///
/// **The four kinds are the whole of Git's object model**, and an id does not say which one it
/// is — the kind is stored in the object's header, not in the id. Reading an object as the wrong
/// kind is [`rivet_core::Error::WrongObjectKind`], not a silent success. See
/// [`rivet_core::ObjectKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ObjectKindArg {
    /// A blob: file contents with no name and no mode.
    Blob,

    /// A tree: a sorted list of names, modes and ids.
    Tree,

    /// A commit: a tree, parents, an author, a committer and a message.
    Commit,

    /// An annotated tag: an object, a kind, a name, a tagger and a message.
    Tag,
}

/// How a conflicting merge writes its conflict markers.
///
/// # Caveats
///
/// **The style changes the bytes of a conflicted file, and therefore the resolution a user
/// performs.** `Merge` writes the familiar two-sided markers; `Diff3` adds the common ancestor
/// between them, which is strictly more information and strictly more lines; `Zd3` omits the
/// ancestor when it equals ours, which is Git's default since 2.35. See
/// [`rivet_core::ConflictStyle`] and `rivet_core::merge::merge_content`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ConflictStyleArg {
    /// `<<<<<<< ours` / `=======` / `>>>>>>> theirs`.
    Merge,

    /// The same, with the merge base shown between `|||||||` and `=======`.
    Diff3,

    /// Diff3 with the base section omitted when it matches ours.
    Zd3,
}

/// When to colourise output.
///
/// # Caveats
///
/// **`Auto` is the only value that depends on the environment**, and it must test `isatty` on
/// the stream being written — not on stdout, which may be the wrong one. `Always` is what makes
/// a diff piped into a file carry escape sequences, which is occasionally what a user wants and
/// never what a script wants. `NO_COLOR` and `TERM=dumb` take precedence over `Auto`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorArg {
    /// Colourise even when the output is not a terminal, unless `NO_COLOR` is set.
    Always,

    /// Colourise only when the output is a terminal (the default).
    Auto,

    /// Never colourise.
    Never,
}

/// The value type `rivet config` should coerce a setting to.
///
/// # Caveats
///
/// **Git's INI dialect stores every value as a string.** `bool` and `int` are interpretations
/// applied at read time (`yes`/`on`/`1` are all true; a bare key with no `=` is true), and a
/// setting that does not parse as the requested type is
/// [`rivet_core::Error::ConfigInvalidValue`], not an empty string. Untyped output must be the
/// *raw* value, not a normalised one, or a round-trip through `rivet config` would rewrite a
/// user's config file with different bytes. See `rivet_core::Config::get_bool` and
/// `rivet_core::Config::get_int`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ConfigTypeArg {
    /// Interpret the value as a Git boolean.
    Bool,

    /// Interpret the value as an integer, honouring Git's `k`/`m`/`g` suffixes.
    Int,

    /// Interpret the value as a path, expanding a leading `~`.
    Path,

    /// Do not interpret the value; print what is in the file.
    Str,
}

impl From<FormatArg> for rivet_core::ObjectFormat {
    /// Phase 4 — the repository object format a `--object-format` value names.
    ///
    /// # Caveats
    ///
    /// Total, and deliberately so: the set of formats Rivet can create is closed, and an
    /// `Option<FormatArg>` that was `None` is resolved by the caller to SHA-1 rather than here —
    /// which keeps "the user did not choose" distinguishable from "the user chose SHA-1" at the
    /// call site where the difference matters for the error message.
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::Sha1 => rivet_core::ObjectFormat::Sha1,
            FormatArg::Sha256 => rivet_core::ObjectFormat::Sha256,
        }
    }
}

impl From<UntrackedModeArg> for rivet_core::UntrackedMode {
    /// Phase 4 — the walk mode a `--untracked-files` value names.
    fn from(value: UntrackedModeArg) -> Self {
        match value {
            UntrackedModeArg::No => rivet_core::UntrackedMode::No,
            UntrackedModeArg::Normal => rivet_core::UntrackedMode::Normal,
            UntrackedModeArg::All => rivet_core::UntrackedMode::All,
        }
    }
}

impl From<DiffAlgorithmArg> for rivet_core::DiffAlgorithm {
    /// Phase 8 — the diff algorithm a `--diff-algorithm` value names.
    fn from(value: DiffAlgorithmArg) -> Self {
        match value {
            DiffAlgorithmArg::Myers => rivet_core::DiffAlgorithm::Myers,
            DiffAlgorithmArg::Histogram => rivet_core::DiffAlgorithm::Histogram,
        }
    }
}

impl From<ObjectKindArg> for rivet_core::ObjectKind {
    /// Phase 1 — the object kind a `--type` value names.
    fn from(value: ObjectKindArg) -> Self {
        match value {
            ObjectKindArg::Blob => rivet_core::ObjectKind::Blob,
            ObjectKindArg::Tree => rivet_core::ObjectKind::Tree,
            ObjectKindArg::Commit => rivet_core::ObjectKind::Commit,
            ObjectKindArg::Tag => rivet_core::ObjectKind::Tag,
        }
    }
}

impl From<ConflictStyleArg> for rivet_core::ConflictStyle {
    /// Phase 9 — the conflict style a `--conflict` value names.
    fn from(value: ConflictStyleArg) -> Self {
        match value {
            ConflictStyleArg::Merge => rivet_core::ConflictStyle::Merge,
            ConflictStyleArg::Diff3 => rivet_core::ConflictStyle::Diff3,
            ConflictStyleArg::Zd3 => rivet_core::ConflictStyle::Zd3,
        }
    }
}
