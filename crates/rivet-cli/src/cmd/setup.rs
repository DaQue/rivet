//! Setup commands: `init`, `clone` and `config`.
//!
//! These are the three commands that must work *without* an existing repository. `init` creates
//! one, `clone` creates one and fills it from a remote, and `config --global` reads and writes a
//! file that belongs to no repository at all. The dispatcher therefore never opens a repository
//! for them; each handler decides what it needs, and [`crate::cmd::open_or_init`] is the shared
//! decision.
//!
//! *Phase 2 for `init`, Phase 11 for `clone`, Phase 3 for `config`.*

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::{ColorArg, ConfigTypeArg, FormatArg, GlobalArgs};

/// Arguments to `rivet init`.
///
/// # Caveats
///
/// * **`format` is permanent.** See [`crate::cli::FormatArg`]: a repository created as SHA-256
///   can never exchange history with a SHA-1 one, and `rivet init --object-format=sha256` in a
///   directory that already holds a SHA-1 repository is a refusal
///   ([`rivet_core::Error::ObjectFormatMismatch`]), not a conversion.
/// * **`directory` is the work tree when the initialisation is not bare**, and `<directory>/.git`
///   is the git directory. Making it the git directory is `--separate-git-dir`, which Rivet does
///   not implement.
/// * **`initial_branch` is written to `HEAD` and nowhere else.** It does not create a ref —
///   a fresh repository has none — which is why a subsequent `rivet branch` correctly lists
///   nothing until the first commit.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// The directory to initialise, or the current directory.
    #[arg(value_name = "DIRECTORY")]
    pub directory: Option<PathBuf>,

    /// Create a bare repository, with no work tree.
    #[arg(long)]
    pub bare: bool,

    /// The object format to create. Cannot be changed later.
    #[arg(long = "object-format", value_name = "FORMAT", value_enum)]
    pub object_format: Option<FormatArg>,

    /// The name of the branch `HEAD` names before the first commit.
    #[arg(short = 'b', long = "initial-branch", value_name = "NAME")]
    pub initial_branch: Option<String>,

    /// Re-initialise an existing repository without complaining.
    #[arg(long)]
    pub force: bool,

    /// Write the repository's objects into a shared directory.
    #[arg(long = "shared", value_name = "PERM")]
    pub shared: Option<String>,

    /// Which of `always`/`auto`/`never` to apply to this repository's output.
    #[arg(long = "color", value_enum, value_name = "WHEN")]
    pub color: Option<ColorArg>,
}

/// Arguments to `rivet clone`.
///
/// # Caveats
///
/// * **A clone is four operations and the order matters**: init, `remote add`, fetch, then
///   check out the remote's `HEAD`. A failure after the fetch but before the checkout leaves a
///   repository that is *correct* and has no work tree files, which is why the message for that
///   state must say how to finish it rather than implying the clone failed outright.
/// * **`bare` and `mirror` differ in which refs are created**, not in whether there is a work
///   tree. A mirror maps every remote ref to the same local name (`refs/heads/x` →
///   `refs/heads/x`) rather than to a remote-tracking ref, so a clone made with `--mirror` is
///   already configured to push everything back.
/// * **`branch` names the branch to check out, not a ref to fetch.** With `--bare` it names
///   which branch `HEAD` points at. A branch that the remote's `HEAD` does not advertise is
///   still reachable and must be found in the advertisement rather than assumed absent.
/// * **Credentials are never printed**, including in the progress output and in any error. The
///   origin URL is stored as given, but every message that echoes it goes through
///   `rivet_core::remote::redact_url` first.
#[derive(Debug, clap::Args)]
pub struct CloneArgs {
    /// The repository to clone from: an HTTPS URL, an SSH URL, a `file://` URL or a local path.
    #[arg(value_name = "REPOSITORY")]
    pub repository: String,

    /// The directory to clone into, or the remote's own name.
    #[arg(value_name = "DIRECTORY")]
    pub directory: Option<PathBuf>,

    /// Clone without a work tree.
    #[arg(long)]
    pub bare: bool,

    /// Mirror every ref rather than creating remote-tracking refs.
    #[arg(long)]
    pub mirror: bool,

    /// Check out this branch instead of the remote's `HEAD`.
    #[arg(short = 'b', long = "branch", value_name = "NAME")]
    pub branch: Option<String>,

    /// Fetch only the tip of history and no earlier commits.
    #[arg(long, value_name = "N")]
    pub depth: Option<u32>,

    /// Copy the objects from a local clone with hard links.
    #[arg(long = "local")]
    pub local: bool,

    /// Fetch all tags, not only those reachable from the fetched branches.
    #[arg(long)]
    pub tags: bool,

    /// Name the remote, instead of `origin`.
    #[arg(
        short = 'o',
        long = "origin",
        value_name = "NAME",
        default_value = "origin"
    )]
    pub origin: String,

    /// The object format, for a local clone; must match the source's.
    #[arg(long = "object-format", value_name = "FORMAT", value_enum)]
    pub object_format: Option<FormatArg>,
}

/// Arguments to `rivet config`.
///
/// # Caveats
///
/// * **The three modes are distinguished by argument count, exactly as in Git**, and the
///   grammar below is what expresses that: `key` alone reads, `key value` writes, and
///   `--unset`, `--list`, `--get-regexp` select the others by flag. A `Mode` field would be
///   clearer to read and impossible to express in `clap` without duplicating every positional.
/// * **`--global` and `--local` are mutually exclusive and one of them must be chosen when a
///   repository is present**, or the write goes to the wrong file. Git's rule is local when
///   inside a repository and global otherwise; `--system` is deliberately not implemented —
///   it is a machine-wide file Rivet has no business writing without being asked.
/// * **Writing a value that the *reader* will interpret differently is not an error.** Setting
///   `core.bare` to the string `maybe` succeeds and fails on the next read with
///   [`rivet_core::Error::ConfigInvalidValue`], because Git's config is a text file that any
///   editor may corrupt and Rivet must not be the tool that refuses to write it back.
/// * **`--type` coerces on read only.** It never rewrites the file, so `rivet config --type=int
///   core.abbrev` prints `7` and leaves the config byte-identical.
#[derive(Debug, clap::Args)]
pub struct ConfigArgs {
    /// The key to read or write, in `section.key` or `section.subsection.key` form.
    #[arg(value_name = "KEY")]
    pub key: Option<String>,

    /// The value to write. Its presence turns a read into a write.
    #[arg(value_name = "VALUE", requires = "key")]
    pub value: Option<String>,

    /// Read and write the per-user file, `~/.gitconfig`.
    #[arg(long = "global")]
    pub global: bool,

    /// Read and write this repository's `.git/config` (the default inside a repository).
    #[arg(long = "local")]
    pub local: bool,

    /// Read and write the machine-wide `/etc/gitconfig`.
    #[arg(long = "system")]
    pub system: bool,

    /// List every setting, or every setting under a section.
    #[arg(short = 'l', long = "list")]
    pub list: bool,

    /// Remove the setting rather than reading or writing it.
    #[arg(long = "unset")]
    pub unset: bool,

    /// Remove every setting whose value matches this one.
    #[arg(long = "unset-all")]
    pub unset_all: bool,

    /// Remove every setting in a section.
    #[arg(long = "remove-section", value_name = "NAME")]
    pub remove_section: Option<String>,

    /// Rename a section.
    #[arg(long = "rename-section", value_name = "OLD", requires = "new_name")]
    pub rename_section: Option<String>,

    /// The new name for `--rename-section`.
    #[arg(long = "new-name", value_name = "NEW")]
    pub new_name: Option<String>,

    /// Print every value whose key matches this regular expression.
    #[arg(long = "get-regexp", value_name = "PATTERN")]
    pub get_regexp: Option<String>,

    /// Interpret the value as this type when reading.
    #[arg(long = "type", value_enum, value_name = "TYPE")]
    pub value_type: Option<ConfigTypeArg>,

    /// Print only the value, with no key and no quoting.
    #[arg(long = "null", short = 'z')]
    pub null_terminated: bool,

    /// Print nothing; exit non-zero unless the key is present.
    #[arg(long = "get", short = 'g')]
    pub get: bool,

    /// Add the value rather than replacing an existing one.
    #[arg(long = "add")]
    pub add: bool,
}

/// Phase 2 — create an empty repository, or re-initialise an existing one.
///
/// # Purpose
///
/// Create the `.git` directory skeleton — `HEAD`, `config`, `objects/`, `refs/heads/`,
/// `refs/tags/` — in the given directory, or re-initialise one that is already there. Prints
/// `Initialized empty Git repository in <path>` (or
/// `Reinitialized existing Git repository in <path>`), and that exact wording is what the
/// differential tests compare against `git init`, so it is not a place for a nicer message.
///
/// # Exit codes
///
/// * `0` — the repository was created or re-initialised.
/// * `128` — the directory could not be created or written, the `--object-format` conflicts
///   with an existing repository's, or the initial branch name is not a legal ref name.
///
/// # Caveats
///
/// * **Re-initialising must not destroy anything.** The config is *merged* and existing refs,
///   objects and the index are left alone; a second `rivet init` in a repository that has
///   commits is a no-op that prints "Reinitialized", not a reset.
/// * **`--object-format` on an existing repository is a check, not a setting.** If the value
///   differs from the repository's own, the command refuses with
///   [`rivet_core::Error::ObjectFormatMismatch`]; it never rewrites a repository's format, and
///   `--force` does not make it.
/// * **The default branch comes from `init.defaultBranch` in the user's config**, falling back
///   to `main` — *not* to Git's historical `master`. The value is read before the repository
///   exists, so it comes from the global and system files only.
/// * **`--bare` in a directory that already has a repository is refused**, because it would
///   reinterpret a work tree that exists. `--force` overrides this and is the only thing
///   `--force` does here.
/// * **`--shared` is accepted and not implemented**: it changes the permissions of the files
///   Rivet creates, which is a filesystem concern this scaffold has no opinion about. It must
///   not be silently ignored in the output — a user who asked for a shared repository needs to
///   know they did not get one.
pub fn run_init(global: &GlobalArgs, args: &InitArgs) -> Result<i32> {
    todo!()
}

/// Phase 11 — clone a repository into a new directory.
///
/// # Purpose
///
/// Create a fresh repository, add the remote, fetch its refs and objects, then check out the
/// remote's default branch — five steps that together produce a working tree identical to the
/// one `git clone` produces from the same URL.
///
/// # Exit codes
///
/// * `0` — the clone completed and the work tree was checked out.
/// * `128` — the URL is unparseable or unreachable, authentication failed, the requested branch
///   does not exist, the destination directory is not empty, or the remote's object format
///   differs from the requested one.
///
/// # Caveats
///
/// * **The destination must be empty or absent.** Cloning into a directory with files in it is
///   refused rather than merged into; a half-clone that overwrote a user's directory is the
///   worst failure this command can have, and the check must happen *before* the fetch, not
///   after.
/// * **A failed fetch must leave no repository behind.** The clone creates its directory and
///   `.git` before it knows the URL is reachable, so the error path has to remove what it
///   created — and only what it created, never a directory that was already there.
/// * **`--depth` is not implemented yet** (plan §12). It must fail loudly rather than quietly
///   fetching full history: a shallow clone that is silently deep costs the user a gigabyte and
///   tells them nothing.
/// * **`--local` is an optimisation, not a different result.** Hard-linking a local clone's
///   objects is only safe because loose objects are written read-only and never modified in
///   place; a clone that copied packs by reference would break the moment either side ran a
///   repack, so `--local` must fall back to a real copy for packed objects.
/// * **The remote's `HEAD` is advisory.** A remote whose `HEAD` is a symref to a branch that was
///   not advertised is normal for an empty repository; the clone succeeds with an unborn `HEAD`
///   rather than failing, matching Git.
/// * **Credentials are never written to the config or printed.** A URL containing a password is
///   stored redacted, and `rivet_core::remote::redact_url` is applied before anything is
///   echoed — including in `--verbose` progress, where it is easiest to leak one.
pub fn run_clone(global: &GlobalArgs, args: &CloneArgs) -> Result<i32> {
    todo!()
}

/// Phase 3 — read and write repository configuration.
///
/// # Purpose
///
/// The general-purpose accessor for Git's INI dialect, over three scopes: the per-user
/// `~/.gitconfig`, the per-repository `.git/config`, and (accepted but never written) the
/// system file. Reads with one positional, writes with two, and removes with `--unset`.
///
/// # Exit codes
///
/// * `0` — a value was read or written, or `--list` produced output.
/// * `1` — the key is not set, for a read (`--get`, a bare key, `--get-regexp`). This is the
///   documented-negative case: Git exits `1`, not `128`, and a script testing
///   `rivet config --get user.email || echo unset` depends on it.
/// * `128` — the config file is malformed, the key is not in `section.key` form, no scope was
///   given and no repository was found, or the value does not parse as the requested `--type`.
///
/// # Caveats
///
/// * **Reading never reformats the file.** The value printed is the bytes between `=` and the
///   line's end with Git's quoting removed, and nothing else — a normalisation here would make
///   `rivet config a.b` disagree with `git config a.b` on any value with a trailing space.
/// * **A subsection is case-sensitive and a key is not.** `remote.Origin.url` and
///   `remote.origin.url` are the same setting; `remote.origin.url` and `remote.ORIGIN.url` are
///   not. Getting this backwards makes `rivet remote -v` silently list nothing.
/// * **The scope precedence is not applied here.** `--global` reads only the global file, not
///   "global and system, merged". The *merged* view is what a repository's own `config()`
///   provides to the rest of the CLI, and it is a different operation from what this command
///   prints.
/// * **`--unset` on a key with multiple values removes all of them and exits `0`**, where
///   `--unset-all` exists to say so explicitly. That is Git's behaviour and it is a footgun;
///   it is preserved rather than fixed because a config-writing script depends on the exit code.
/// * **Writes go through a lock file and a rename.** A concurrent `git config` must never lose
///   its write, and a partial write must never leave a truncated config — a corrupt
///   `.git/config` makes the repository unopenable.
/// * **`--system` requires a path Rivet will not guess.** Reading it is fine; writing it is
///   refused with a message naming the file, because `/etc/gitconfig` is outside the user's own
///   and outside this repository's.
pub fn run_config(global: &GlobalArgs, args: &ConfigArgs) -> Result<i32> {
    todo!()
}
